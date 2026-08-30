use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::App;
use crate::automatic_selection::{
    AutoSelectionExplanation, AutoSelectionPlan, AutoSelectionReason, NodeQualityFacts,
    NodeViewProjection, PanelMembership, ReachabilityTier, SelectionScope,
};
use crate::benchmark_workflow::{
    BenchmarkCompletion, BenchmarkStart, BenchmarkUpdate, SustainedKind,
    assessment_is_quick_eligible,
};
use crate::controller::{BenchmarkRequest, NodeReachabilityAssessment, ProbeOutcome, ProxyGroup};
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
            self.automatic_selection_state = Default::default();
            self.active_node_traffic = Default::default();
            self.last_auto_selection_explanation = None;
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
        self.auto_select_node_view = self.node_view_panel.id();
        self.auto_select_ranking_policy = self.active_node_view_ranking_policy();
        self.automatic_selection_state = Default::default();
        self.active_node_traffic = Default::default();
        self.last_auto_selection_explanation = None;
        self.last_auto_select_benchmark = None;
        self.save_runtime_state()?;
        if self.background_worker_management_enabled() {
            let worker = self.ensure_auto_pick_background_worker()?;
            self.set_status_only(format!(
                "Auto-pick enabled for {} [{} / {}] via background worker pid {} ({}, 20% material gate, two-round confirmation, every {}s)",
                group_name,
                self.auto_select_node_view,
                self.auto_select_ranking_policy.label(),
                worker.pid(),
                self.benchmark_scope_label(),
                self.auto_select_interval.as_secs()
            ));
        } else {
            self.set_status_only(format!(
                "Auto-pick enabled for {} [{} / {}] ({}, 20% material gate, two-round confirmation, every {}s)",
                group_name,
                self.auto_select_node_view,
                self.auto_select_ranking_policy.label(),
                self.benchmark_scope_label(),
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
        let candidate_names = self.auto_selection_evidence_members_for_group(&group);
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

    pub(super) fn active_auto_selection_scope(&self) -> Option<SelectionScope> {
        if !self.auto_select_enabled {
            return None;
        }
        let group = self.auto_select_group()?;
        let current_node = group.current.clone()?;
        let panel_revision = self.cached_auto_selection_panel_revision(group);
        Some(SelectionScope {
            quality_generation: self
                .benchmark_workflow
                .runtime_receipt()?
                .quality_generation(),
            selector: group.name.clone(),
            panel: self.auto_select_node_view.clone(),
            panel_revision,
            current_node,
        })
    }

    fn cached_auto_selection_panel_revision(&self, group: &ProxyGroup) -> u64 {
        if self.auto_select_node_view == crate::automatic_selection::NodeViewId::current_selector()
            || self.auto_select_node_view == crate::automatic_selection::NodeViewId::streaming()
        {
            return 0;
        }
        self.cached_custom_node_view_projection(
            &self.auto_select_node_view,
            &group.name,
            &group.members,
        )
        .revision
    }

    fn auto_selection_evidence_members_for_group(&self, group: &ProxyGroup) -> Vec<String> {
        let built_in_view = self.auto_select_node_view
            == crate::automatic_selection::NodeViewId::current_selector()
            || self.auto_select_node_view == crate::automatic_selection::NodeViewId::streaming();
        // Decision projection stays Included-only, but evidence acquisition is intentionally
        // wider: an Untested Streaming node must be probed before it can ever earn membership.
        // Future manifest views can provide their own discovery projection instead of inheriting
        // this built-in selector-wide behavior.
        let mut candidates = if built_in_view {
            self.benchmark_candidates_for_group(group)
        } else {
            let projection = self.cached_custom_node_view_projection(
                &self.auto_select_node_view,
                &group.name,
                &group.members,
            );
            group
                .members
                .iter()
                .filter(|node| projection.membership(node) == PanelMembership::Included)
                .cloned()
                .collect()
        };
        // The current node may sit outside a custom panel. Probe it for failure/traffic comparison,
        // but the decision projection below remains Included-only and can never select it merely
        // because evidence acquisition added it here.
        if let Some(current) = group
            .current
            .as_ref()
            .filter(|current| group.members.contains(*current))
            && !candidates.contains(current)
        {
            candidates.insert(0, current.clone());
        }
        candidates
    }

    fn built_in_auto_selection_panel_projection(
        &self,
        group: &ProxyGroup,
        facts: &[NodeQualityFacts],
    ) -> NodeViewProjection {
        let members = group
            .members
            .iter()
            .map(|node| {
                let membership = if !self.member_matches_filter(node) {
                    PanelMembership::Rejected
                } else if self.auto_select_node_view
                    == crate::automatic_selection::NodeViewId::current_selector()
                {
                    PanelMembership::Included
                } else if self.auto_select_node_view
                    == crate::automatic_selection::NodeViewId::streaming()
                {
                    match facts.iter().find(|facts| facts.node == *node) {
                        Some(facts)
                            if facts.reachability.is_some_and(|tier| tier.successes() >= 2)
                                && facts.throughput_bytes_per_second.is_some() =>
                        {
                            PanelMembership::Included
                        }
                        Some(facts) if facts.reachability.is_none() => {
                            if facts.recent_quick_rounds == 0 {
                                PanelMembership::Untested
                            } else {
                                PanelMembership::Incomplete
                            }
                        }
                        Some(_) => PanelMembership::Rejected,
                        None => PanelMembership::Untested,
                    }
                } else {
                    // Unknown IDs belong to future manifest panels. Until their projection is
                    // registered, fail closed instead of silently expanding to every selector node.
                    PanelMembership::Untested
                };
                (node.clone(), membership)
            })
            .collect::<BTreeMap<_, _>>();
        NodeViewProjection {
            id: self.auto_select_node_view.clone(),
            label: match self.auto_select_node_view.as_str() {
                crate::automatic_selection::CURRENT_SELECTOR_VIEW_ID => {
                    "Current selector".to_string()
                }
                crate::automatic_selection::STREAMING_VIEW_ID => "Streaming".to_string(),
                id => id.to_string(),
            },
            ranking_policy: self.auto_select_ranking_policy,
            revision: 0,
            members,
        }
    }

    pub(super) fn finish_auto_select_benchmark(
        &mut self,
        group_name: &str,
        round_id: u64,
        assessments: &[NodeReachabilityAssessment],
        quality_current: bool,
    ) -> Result<()> {
        if !quality_current {
            self.defer_auto_selection_after_quality_change(
                group_name,
                "managed runtime changed before completion".to_string(),
            );
            return Ok(());
        }
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
        let Some(current_node) = group.current.clone() else {
            self.set_status_only(format!(
                "Auto-pick deferred for {group_name}: current node unset"
            ));
            return Ok(());
        };

        let quality_lease = match self.benchmark_workflow.acquire_auto_selection_read_lease() {
            Ok(lease) => lease,
            Err(error) => {
                self.defer_auto_selection_after_quality_change(group_name, format!("{error:#}"));
                return Ok(());
            }
        };
        let mut facts = match self.benchmark_workflow.node_quality_facts_with_lease(
            &quality_lease,
            group_name,
            &group.members,
        ) {
            Ok(facts) => facts,
            Err(error) => {
                self.defer_auto_selection_after_quality_change(group_name, format!("{error:#}"));
                return Ok(());
            }
        };
        let current_round = assessments
            .iter()
            .map(|assessment| (assessment.name.as_str(), assessment))
            .collect::<BTreeMap<_, _>>();
        for facts in &mut facts {
            facts.reachability = current_round
                .get(facts.node.as_str())
                .and_then(|assessment| reachability_tier(assessment));
        }
        let panel = if self.auto_select_node_view
            == crate::automatic_selection::NodeViewId::current_selector()
            || self.auto_select_node_view == crate::automatic_selection::NodeViewId::streaming()
        {
            self.built_in_auto_selection_panel_projection(&group, &facts)
        } else {
            // The render cache is never authority for a selector write. Re-read custom membership
            // under the same generation lease that `AutoSelectionPlan` keeps alive through every
            // controller PUT, so reconciliation cannot invalidate the chosen panel mid-action.
            match self.leased_custom_node_view_projection(
                &quality_lease,
                &self.auto_select_node_view,
                group_name,
                &group.members,
            ) {
                Ok(panel) => panel,
                Err(error) => {
                    self.defer_auto_selection_after_quality_change(
                        group_name,
                        format!("custom panel projection unavailable: {error:#}"),
                    );
                    return Ok(());
                }
            }
        };
        let parent_switch = self.implicit_root_parent_switch_for_group(group_name);
        let scope = SelectionScope {
            quality_generation: quality_lease.generation(),
            selector: group_name.to_string(),
            panel: panel.id.clone(),
            panel_revision: panel.revision,
            current_node,
        };
        let transfer = self.active_node_traffic.status(&scope, Instant::now());
        let decision = self.automatic_selection_state.evaluate(
            scope.clone(),
            round_id,
            parent_switch.is_some(),
            &panel,
            &facts,
            transfer,
        );
        self.last_auto_selection_explanation = Some(AutoSelectionExplanation::new(
            group_name,
            panel.id.clone(),
            &decision.reason,
        ));
        let parent_switch = decision.activate_route.then_some(parent_switch).flatten();
        let plan = AutoSelectionPlan::new(decision, parent_switch, quality_lease);
        let detail = plan.decision.reason.detail();
        let mut status = if plan.decision.target_node.is_none() && plan.parent_switch.is_none() {
            format!("Auto-pick {} [{}]: {detail}", group_name, panel.label)
        } else {
            // Keep `plan` intact until both controller writes return: its read lease is the proof
            // that ranked facts cannot be reconciled between selection and route activation.
            if let Some(target) = &plan.decision.target_node {
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
            match (&plan.decision.target_node, &plan.parent_switch) {
                (Some(target), Some((_, route_group))) => format!(
                    "Auto-pick switched {} to {} and selected {}: {}",
                    group_name, target, route_group, detail
                ),
                (Some(target), None) => format!(
                    "Auto-pick switched {} to {}: {}",
                    group_name, target, detail
                ),
                (None, Some((_, route_group))) => format!(
                    "Auto-pick selected {}; kept {} on {}: {}",
                    route_group,
                    group_name,
                    group.current.as_deref().unwrap_or("unset"),
                    detail
                ),
                (None, None) => unreachable!("action branch requires a controller write"),
            }
        };

        let duplicate_round = matches!(
            plan.decision.reason,
            AutoSelectionReason::DuplicateRound { .. }
        );
        drop(plan);
        if !duplicate_round {
            let sustained_members = self.auto_selection_evidence_members_for_group(&group);
            let nodes = self.benchmark_workflow.automatic_sustained_candidates(
                group_name,
                Some(&scope.current_node),
                &sustained_members,
                assessments,
            );
            match self.start_sustained_nodes(
                group_name.to_string(),
                nodes.clone(),
                SustainedKind::Automatic,
            ) {
                Ok(BenchmarkStart::Started) => status.push_str(&format!(
                    "; sustained probing {} evidence candidate(s) in background",
                    nodes.len()
                )),
                Ok(_) => {}
                Err(error) => status.push_str(&format!(
                    "; sustained probing deferred after runtime change: {error}"
                )),
            }
        }
        self.set_status_only(status);
        Ok(())
    }

    fn defer_auto_selection_after_quality_change(&mut self, group: &str, detail: String) {
        let reason = AutoSelectionReason::QualityFactsUnavailable { detail };
        self.set_status_only(format!(
            "Auto-pick deferred for {group}: {}",
            reason.detail()
        ));
        self.last_auto_selection_explanation = Some(AutoSelectionExplanation::new(
            group,
            self.auto_select_node_view.clone(),
            &reason,
        ));
        self.automatic_selection_state = Default::default();
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
            BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
                group,
                round_id,
                assessments,
                quality_current,
            }) => {
                self.finish_auto_select_benchmark(&group, round_id, &assessments, quality_current)?
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

fn reachability_tier(assessment: &NodeReachabilityAssessment) -> Option<ReachabilityTier> {
    assessment.assessment.map(|_| {
        ReachabilityTier::from_successes(
            assessment
                .attempts
                .iter()
                .filter(|outcome| matches!(outcome, ProbeOutcome::Reachable { .. }))
                .count() as u8,
        )
    })
}

#[cfg(test)]
#[path = "tui_benchmark_workflow_tests.rs"]
mod tests;
