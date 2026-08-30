use std::collections::BTreeMap;

use anyhow::Result;

use crate::automatic_selection::{NodeViewId, NodeViewProjection, PanelMembership, RankingPolicy};
use crate::storage::{NodeQualityReadLease, StoredUsabilityProbeRun, UsabilityProbeFactRecord};
use crate::usability_probe::{
    UsabilityProbeJob, UsabilityProbeJobEvent, UsabilityProbeNodeResult, spawn_usability_probe_job,
    usability_presentation_text,
};

use super::App;
use super::view::NodeViewPanel;

pub(super) struct ActiveUsabilityProbe {
    manifest_id: NodeViewId,
    selector: String,
    run_id: i64,
    generation: u64,
    selector_member_count: usize,
    received: BTreeMap<String, UsabilityProbeNodeResult>,
    job: UsabilityProbeJob,
}

impl App {
    pub(super) fn start_manual_usability_probe(&mut self) {
        if self.usability_probe_job.is_some() {
            self.set_status_only("A custom usability probe is already running");
            return;
        }
        let manifest_id = match &self.node_view_panel {
            NodeViewPanel::Custom(id) => id.clone(),
            _ => {
                self.set_status_only("Select a custom node-view tab before pressing U");
                return;
            }
        };
        let Some(manifest) = self
            .usability_probe_manifests
            .iter()
            .find(|manifest| manifest.id == manifest_id)
            .cloned()
        else {
            self.set_status_only("The selected usability manifest is no longer available");
            return;
        };
        let Some(group) = self.selected_member_panel_group().cloned() else {
            self.set_status_only("No selector group is available for the usability probe");
            return;
        };
        let (run_id, generation) = match self
            .benchmark_workflow
            .begin_usability_probe_run(manifest.id.as_str(), &group.name)
        {
            Ok(ticket) => ticket,
            Err(error) => {
                self.set_status_only(format!(
                    "Cannot run {} usability probe: {error:#}",
                    manifest.label
                ));
                return;
            }
        };
        let job = match spawn_usability_probe_job(
            manifest.clone(),
            group.members.clone(),
            self.client.base_url.clone(),
            self.client.client.clone(),
        ) {
            Ok(job) => job,
            Err(error) => {
                let diagnostic = format!("failed to launch usability probe: {error:#}");
                let _ = self.benchmark_workflow.finish_usability_probe_run(
                    run_id,
                    generation,
                    false,
                    None,
                    Some(&diagnostic),
                    &[],
                );
                self.set_status_only(format!("Cannot start {}: {error:#}", manifest.label));
                return;
            }
        };
        self.usability_probe_job = Some(ActiveUsabilityProbe {
            manifest_id,
            selector: group.name,
            run_id,
            generation,
            selector_member_count: group.members.len(),
            received: BTreeMap::new(),
            job,
        });
        self.set_status_only(format!("Running {} usability probe...", manifest.label));
    }

    pub(super) fn poll_usability_probe_updates(&mut self) {
        let mut events = Vec::new();
        if let Some(active) = self.usability_probe_job.as_ref() {
            while let Some(event) = active.job.try_recv() {
                let finished = matches!(event, UsabilityProbeJobEvent::Finished(_));
                events.push(event);
                if finished {
                    break;
                }
            }
        }
        for event in events {
            match event {
                UsabilityProbeJobEvent::Progress(result) => {
                    let latest_result = result.clone();
                    let (manifest_id, received_count, selector_member_count) = {
                        let Some(active) = self.usability_probe_job.as_mut() else {
                            continue;
                        };
                        active.received.insert(result.node.clone(), result);
                        (
                            active.manifest_id.clone(),
                            active.received.len(),
                            active.selector_member_count,
                        )
                    };
                    let label = self
                        .usability_probe_manifests
                        .iter()
                        .find(|manifest| manifest.id == manifest_id)
                        .map(|manifest| manifest.label.as_str())
                        .unwrap_or("Custom");
                    self.set_status_only(usability_probe_progress_status(
                        label,
                        received_count,
                        selector_member_count,
                        &latest_result,
                    ));
                }
                UsabilityProbeJobEvent::Finished(completion) => {
                    let Some(mut active) = self.usability_probe_job.take() else {
                        continue;
                    };
                    active.job.join();
                    let facts = completion
                        .results
                        .iter()
                        .map(|result| UsabilityProbeFactRecord {
                            node: result.node.clone(),
                            usable: result.usable,
                            detail: result.detail.clone(),
                        })
                        .collect::<Vec<_>>();
                    let persisted = self.benchmark_workflow.finish_usability_probe_run(
                        active.run_id,
                        active.generation,
                        completion.complete,
                        completion.summary.as_deref(),
                        completion.diagnostic.as_deref(),
                        &facts,
                    );
                    let label = self
                        .usability_probe_manifests
                        .iter()
                        .find(|manifest| manifest.id == active.manifest_id)
                        .map(|manifest| manifest.label.clone())
                        .unwrap_or_else(|| "Custom".to_string());
                    match persisted {
                        Ok(true) => {
                            self.refresh_cached_usability_run(
                                &active.manifest_id,
                                &active.selector,
                            );
                            self.sync_selection_to_displayed_members();
                            let usable = facts.iter().filter(|fact| fact.usable).count();
                            let summary = usability_status_suffix(completion.summary.as_deref());
                            let worker_suffix = if self.auto_select_enabled
                                && self.auto_select_node_view == active.manifest_id
                            {
                                self.ensure_auto_pick_background_worker_after_state_change()
                                    .err()
                                    .map(|error| {
                                        format!(
                                            "; background worker refresh deferred: {error:#}"
                                        )
                                    })
                                    .unwrap_or_default()
                            } else {
                                String::new()
                            };
                            self.set_status_only(format!(
                                "{label} probe complete: {usable}/{} reported nodes usable{summary}{worker_suffix}",
                                facts.len(),
                            ));
                        }
                        Ok(false) => self.set_status_only(format!(
                            "{label} probe incomplete; the previous complete result was preserved{}",
                            usability_status_suffix(completion.diagnostic.as_deref())
                        )),
                        Err(error) => self.set_status_only(format!(
                            "{label} probe could not publish results: {error:#}"
                        )),
                    }
                }
            }
        }
    }

    pub(super) fn cancel_active_usability_probe(&mut self) {
        let Some(mut active) = self.usability_probe_job.take() else {
            return;
        };
        active.job.cancel();
        active.job.join();
        let facts = active
            .received
            .into_values()
            .map(|result| UsabilityProbeFactRecord {
                node: result.node,
                usable: result.usable,
                detail: result.detail,
            })
            .collect::<Vec<_>>();
        let _ = self.benchmark_workflow.finish_usability_probe_run(
            active.run_id,
            active.generation,
            false,
            None,
            Some("TUI shutdown cancelled the usability probe"),
            &facts,
        );
    }

    pub(super) fn custom_usability_run(
        &self,
        manifest_id: &NodeViewId,
        selector: &str,
        selector_members: &[String],
    ) -> Option<StoredUsabilityProbeRun> {
        let run = self
            .usability_probe_projection_cache
            .get(&(manifest_id.clone(), selector.to_string()))
            .cloned()?;
        let allowed = selector_members
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        let mut run = run;
        run.results.retain(|result| allowed.contains(&result.node));
        Some(run)
    }

    pub(super) fn cached_custom_node_view_projection(
        &self,
        manifest_id: &NodeViewId,
        selector: &str,
        selector_members: &[String],
    ) -> NodeViewProjection {
        let run = self.custom_usability_run(manifest_id, selector, selector_members);
        self.custom_node_view_projection_from_run(manifest_id, selector_members, run.as_ref())
    }

    pub(super) fn leased_custom_node_view_projection(
        &self,
        lease: &NodeQualityReadLease,
        manifest_id: &NodeViewId,
        selector: &str,
        selector_members: &[String],
    ) -> Result<NodeViewProjection> {
        let run = if self
            .usability_probe_manifests
            .iter()
            .any(|manifest| &manifest.id == manifest_id)
        {
            let persisted = self
                .benchmark_workflow
                .latest_usability_probe_run_with_lease(
                    lease,
                    manifest_id.as_str(),
                    selector,
                    selector_members,
                );
            #[cfg(test)]
            if self
                .benchmark_workflow
                .allows_unpersisted_quality_for_test()
            {
                self.custom_usability_run(manifest_id, selector, selector_members)
            } else {
                persisted?
            }
            #[cfg(not(test))]
            persisted?
        } else {
            None
        };
        Ok(self.custom_node_view_projection_from_run(manifest_id, selector_members, run.as_ref()))
    }

    fn custom_node_view_projection_from_run(
        &self,
        manifest_id: &NodeViewId,
        selector_members: &[String],
        run: Option<&StoredUsabilityProbeRun>,
    ) -> NodeViewProjection {
        let manifest = self
            .usability_probe_manifests
            .iter()
            .find(|manifest| &manifest.id == manifest_id);
        let results = run
            .map(|run| {
                run.results
                    .iter()
                    .map(|result| (result.node.as_str(), result.usable))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        // Selector membership is the outer authority. Iterating it (instead of result rows)
        // prevents a surviving fact from another selector or an old identity from entering the
        // panel, while missing rows remain explicitly Untested rather than silently rejected.
        let members = selector_members
            .iter()
            .map(|node| {
                let membership = if !self.member_matches_filter(node) {
                    PanelMembership::Rejected
                } else {
                    match results.get(node.as_str()) {
                        Some(true) => PanelMembership::Included,
                        Some(false) => PanelMembership::Rejected,
                        None => PanelMembership::Untested,
                    }
                };
                (node.clone(), membership)
            })
            .collect();
        NodeViewProjection {
            id: manifest_id.clone(),
            label: manifest
                .map(|manifest| manifest.label.clone())
                .unwrap_or_else(|| format!("Unavailable custom criterion ({manifest_id})")),
            ranking_policy: manifest
                .map(|manifest| manifest.ranking_policy)
                .unwrap_or(RankingPolicy::Balanced),
            revision: run
                .and_then(|run| u64::try_from(run.run_id).ok())
                .unwrap_or(0),
            members,
        }
    }

    pub(super) fn refresh_usability_probe_projection_cache(&mut self) {
        #[cfg(test)]
        if self
            .benchmark_workflow
            .allows_unpersisted_quality_for_test()
        {
            // Test apps inject complete projection snapshots directly so they can model a run
            // transition without opening a second SQLite store behind the fixture's back.
            return;
        }

        let requests = self
            .usability_probe_manifests
            .iter()
            .flat_map(|manifest| {
                self.groups.iter().map(|group| {
                    (
                        manifest.id.clone(),
                        group.name.clone(),
                        group.members.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        let mut refreshed = BTreeMap::new();
        for (criterion, selector, members) in requests {
            match self.benchmark_workflow.latest_usability_probe_run(
                criterion.as_str(),
                &selector,
                &members,
            ) {
                Ok(Some(run)) => {
                    refreshed.insert((criterion, selector), run);
                }
                Ok(None) => {}
                Err(error) => {
                    // Cache replacement is all-or-nothing. A transient read failure must not turn
                    // every previously rendered custom panel into an empty/untested projection.
                    eprintln!(
                        "warning: failed to refresh usability projection cache; keeping prior projection: {error:#}"
                    );
                    return;
                }
            }
        }
        // Rendering happens every 250 ms. Keeping this cache replacement at controller/config
        // lifecycle boundaries prevents the dashboard from acquiring a reconciliation lock and
        // issuing one SQLite query per custom tab on every frame.
        self.usability_probe_projection_cache = refreshed;
    }

    fn refresh_cached_usability_run(&mut self, criterion: &NodeViewId, selector: &str) {
        if !self
            .usability_probe_manifests
            .iter()
            .any(|manifest| &manifest.id == criterion)
        {
            return;
        }
        let criterion = criterion.clone();
        let Some(members) = self
            .groups
            .iter()
            .find(|group| group.name == selector)
            .map(|group| group.members.clone())
        else {
            self.usability_probe_projection_cache
                .remove(&(criterion, selector.to_string()));
            return;
        };
        match self.benchmark_workflow.latest_usability_probe_run(
            criterion.as_str(),
            selector,
            &members,
        ) {
            Ok(Some(run)) => {
                self.usability_probe_projection_cache
                    .insert((criterion, selector.to_string()), run);
            }
            Ok(None) | Err(_) => {
                self.usability_probe_projection_cache
                    .remove(&(criterion, selector.to_string()));
            }
        }
    }

    pub(super) fn custom_usability_probe_progress(
        &self,
        manifest_id: &NodeViewId,
        selector: &str,
    ) -> Option<(usize, usize)> {
        self.usability_probe_job.as_ref().and_then(|active| {
            (&active.manifest_id == manifest_id && active.selector == selector)
                .then_some((active.received.len(), active.selector_member_count))
        })
    }
}

fn usability_probe_progress_status(
    label: &str,
    received: usize,
    total: usize,
    result: &UsabilityProbeNodeResult,
) -> String {
    let node = usability_presentation_text(&result.node);
    let verdict = if result.usable { "usable" } else { "rejected" };
    let detail = usability_status_suffix(result.detail.as_deref());
    format!("{label} probe: {received}/{total}; latest {node}: {verdict}{detail}")
}

fn usability_status_suffix(value: Option<&str>) -> String {
    value
        .map(usability_presentation_text)
        .filter(|value| !value.is_empty())
        .map(|value| format!(": {value}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progressive_status_shows_latest_result_without_terminal_controls() {
        let status = usability_probe_progress_status(
            "Agy Gemini",
            2,
            5,
            &UsabilityProbeNodeResult {
                node: "node-a".to_string(),
                usable: false,
                detail: Some("login\nfailed\t\u{1b}[31m".to_string()),
            },
        );

        assert!(status.contains("2/5"));
        assert!(status.contains("latest node-a: rejected"));
        assert!(status.contains("login failed \u{fffd}[31m"));
        assert!(!status.chars().any(char::is_control));
    }

    #[test]
    fn terminal_summary_suffix_is_visible_and_printable() {
        let suffix = usability_status_suffix(Some("fixture\ncomplete\u{1b}[0m"));
        assert!(suffix.contains("fixture complete\u{fffd}[0m"));
        assert!(!suffix.chars().any(char::is_control));
    }
}
