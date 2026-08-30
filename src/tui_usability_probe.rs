use std::collections::BTreeMap;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

use crate::automatic_selection::{NodeViewId, NodeViewProjection, PanelMembership, RankingPolicy};
use crate::storage::{
    NodeQualityReadLease, StoredUsabilityProbeRun, UsabilityProbeFactRecord,
    UsabilityProbeRunFinalization,
};
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
    result_ttl: Option<Duration>,
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
        if let Err(error) = self.start_usability_probe(manifest.clone(), group, false) {
            self.set_status_only(format!(
                "Cannot run {} usability probe: {error:#}",
                manifest.label
            ));
        }
    }

    fn start_usability_probe(
        &mut self,
        manifest: crate::usability_probe::UsabilityProbeManifest,
        group: crate::controller::ProxyGroup,
        background: bool,
    ) -> Result<()> {
        if self.usability_probe_job.is_some() {
            bail!("a custom usability probe is already running");
        }
        let (run_id, generation) = self
            .benchmark_workflow
            .begin_usability_probe_run(manifest.id.as_str(), &group.name)?;
        let job = spawn_usability_probe_job(
            manifest.clone(),
            group.members.clone(),
            self.client.base_url.clone(),
            self.client.client.clone(),
        )
        .map_err(|error| {
            let diagnostic = format!("failed to launch usability probe: {error:#}");
            let _ = self.benchmark_workflow.finish_usability_probe_run_with_ttl(
                UsabilityProbeRunFinalization {
                    run_id,
                    generation,
                    complete: false,
                    summary: None,
                    diagnostic: Some(&diagnostic),
                    facts: &[],
                    result_ttl: manifest.result_ttl,
                },
            );
            error
        })?;
        self.usability_probe_job = Some(ActiveUsabilityProbe {
            manifest_id: manifest.id.clone(),
            selector: group.name,
            run_id,
            generation,
            selector_member_count: group.members.len(),
            received: BTreeMap::new(),
            result_ttl: manifest.result_ttl,
            job,
        });
        self.set_status_only(format!(
            "Running {} usability probe{}...",
            manifest.label,
            if background { " in background" } else { "" }
        ));
        Ok(())
    }

    pub(super) fn toggle_background_usability_probe(&mut self) -> Result<()> {
        let NodeViewPanel::Custom(manifest_id) = &self.node_view_panel else {
            self.set_status_only("Select a custom node-view tab before pressing P");
            return Ok(());
        };
        let manifest_id = manifest_id.clone();
        let Some(manifest) = self
            .usability_probe_manifests
            .iter()
            .find(|manifest| manifest.id == manifest_id)
        else {
            self.set_status_only("The selected usability manifest is no longer available");
            return Ok(());
        };
        if !manifest.background {
            self.set_status_only(format!(
                "{} manifest does not permit background execution",
                manifest.label
            ));
            return Ok(());
        }
        if self.background_probe_enabled.remove(&manifest_id) {
            self.background_probe_selectors.remove(&manifest_id);
            self.last_background_probe_started
                .retain(|(id, _), _| id != &manifest_id);
            self.set_status_only(format!("Disabled scheduled {} probe", manifest.label));
        } else {
            let selector = self
                .selected_member_panel_group()
                .map(|group| group.name.clone())
                .ok_or_else(|| anyhow::anyhow!("no selector is available for scheduling"))?;
            self.background_probe_enabled.insert(manifest_id.clone());
            self.background_probe_selectors
                .insert(manifest_id, selector.clone());
            self.set_status_only(format!(
                "Enabled scheduled {} probe for {selector} every {}s",
                manifest.label,
                manifest.background_interval().as_secs()
            ));
        }
        self.save_runtime_state()?;
        self.ensure_auto_pick_background_worker_after_state_change()?;
        Ok(())
    }

    pub(super) fn maybe_start_scheduled_usability_probe(&mut self, now: Instant) {
        if self.usability_probe_job.is_some() {
            return;
        }
        let due = self
            .background_probe_enabled
            .iter()
            .filter_map(|id| {
                let manifest = self
                    .usability_probe_manifests
                    .iter()
                    .find(|manifest| &manifest.id == id && manifest.background)?;
                let selector = self.background_probe_selectors.get(id)?;
                let key = (id.clone(), selector.clone());
                let due = self
                    .last_background_probe_started
                    .get(&key)
                    .is_none_or(|last| now.duration_since(*last) >= manifest.background_interval());
                due.then_some((manifest.clone(), selector.clone(), key))
            })
            .next();
        let Some((manifest, selector, key)) = due else {
            return;
        };
        let Some(group) = self
            .groups
            .iter()
            .find(|group| group.name == selector)
            .cloned()
        else {
            self.set_status_only(format!(
                "Scheduled {} probe deferred: selector {selector} is unavailable",
                manifest.label
            ));
            return;
        };
        // Record the attempt before launch. A broken executable must respect the same interval as
        // a successful run; otherwise a persistent launch failure becomes an unbounded retry loop.
        self.last_background_probe_started.insert(key, now);
        if let Err(error) = self.start_usability_probe(manifest.clone(), group, true) {
            self.set_status_only(format!(
                "Scheduled {} probe could not start: {error:#}",
                manifest.label
            ));
        }
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
                    let persisted = self.benchmark_workflow.finish_usability_probe_run_with_ttl(
                        UsabilityProbeRunFinalization {
                            run_id: active.run_id,
                            generation: active.generation,
                            complete: completion.complete,
                            summary: completion.summary.as_deref(),
                            diagnostic: completion.diagnostic.as_deref(),
                            facts: &facts,
                            result_ttl: active.result_ttl,
                        },
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
        let _ = self.benchmark_workflow.finish_usability_probe_run_with_ttl(
            UsabilityProbeRunFinalization {
                run_id: active.run_id,
                generation: active.generation,
                complete: false,
                summary: None,
                diagnostic: Some("TUI shutdown cancelled the usability probe"),
                facts: &facts,
                result_ttl: active.result_ttl,
            },
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

    pub(super) fn custom_usability_run_is_expired(&self, run: &StoredUsabilityProbeRun) -> bool {
        usability_run_expired(run, current_time_ms())
    }

    pub(super) fn custom_usability_latest_failure(
        &self,
        run: &StoredUsabilityProbeRun,
    ) -> Option<String> {
        run.latest_attempt.as_ref().and_then(|attempt| {
            (!attempt.complete && attempt.run_id != run.run_id).then(|| {
                format!(
                    "run #{} failed{}",
                    attempt.run_id,
                    attempt
                        .diagnostic
                        .as_deref()
                        .map(|detail| format!(": {}", usability_presentation_text(detail)))
                        .unwrap_or_default()
                )
            })
        })
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
        let expired = run.is_some_and(|run| usability_run_expired(run, current_time_ms()));
        let results = (!expired)
            .then_some(run)
            .flatten()
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
                .map(|manifest| {
                    if expired {
                        format!("{} (expired)", manifest.label)
                    } else {
                        manifest.label.clone()
                    }
                })
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

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn usability_run_expired(run: &StoredUsabilityProbeRun, now_ms: u64) -> bool {
    run.expires_at_ms.is_some_and(|expires| now_ms >= expires)
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
    use crate::automatic_selection::{NodeViewId, RankingPolicy};
    use crate::usability_probe::{UsabilityProbeManifest, UsabilityProbeSource};

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

    #[test]
    fn scheduled_probe_authorization_requires_manifest_permission_and_user_toggle() {
        let mut app = super::super::test_support::test_app();
        let permitted = NodeViewId::new("permitted").unwrap();
        let denied = NodeViewId::new("denied").unwrap();
        let manifest = |id: NodeViewId, background| UsabilityProbeManifest {
            id,
            label: "Fixture".to_string(),
            ranking_policy: RankingPolicy::Balanced,
            source: UsabilityProbeSource::Url("https://example.test/".to_string()),
            background,
            interval: Some(Duration::from_secs(60)),
            result_ttl: None,
            timeout: Duration::from_secs(30),
            source_path: std::path::PathBuf::from("fixture.json"),
        };
        app.usability_probe_manifests = vec![
            manifest(permitted.clone(), true),
            manifest(denied.clone(), false),
        ];

        app.node_view_panel = NodeViewPanel::Custom(permitted.clone());
        app.toggle_background_usability_probe()
            .expect("explicitly authorize permitted manifest");
        assert!(app.background_probe_enabled.contains(&permitted));
        assert_eq!(
            app.background_probe_selectors
                .get(&permitted)
                .map(String::as_str),
            Some("select")
        );

        app.node_view_panel = NodeViewPanel::Custom(denied.clone());
        app.toggle_background_usability_probe()
            .expect("deny manifest without background permission");
        assert!(!app.background_probe_enabled.contains(&denied));

        app.node_view_panel = NodeViewPanel::Custom(permitted.clone());
        app.toggle_background_usability_probe()
            .expect("explicitly revoke permitted manifest");
        assert!(!app.background_probe_enabled.contains(&permitted));
        assert!(!app.background_probe_selectors.contains_key(&permitted));
    }
}
