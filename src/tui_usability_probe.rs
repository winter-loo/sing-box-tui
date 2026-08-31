use std::collections::BTreeMap;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::automatic_selection::{NodeViewId, NodeViewProjection, PanelMembership, RankingPolicy};
use crate::builtin_usability_probe::{AGY_EXECUTABLE_ENV, BuiltinProbeContext};
use crate::storage::{
    NodeQualityReadLease, StoredUsabilityProbeRun, UsabilityProbeFactRecord,
    UsabilityProbeRunFinalization,
};
use crate::usability_probe::{
    UsabilityProbeExecutionContext, UsabilityProbeJob, UsabilityProbeJobEvent,
    UsabilityProbeNodeResult, UsabilityProbeProgress, spawn_usability_probe_job_with_context,
    usability_presentation_text,
};

use super::App;
use super::view::NodeViewPanel;

pub(super) struct ActiveUsabilityProbe {
    manifest_id: NodeViewId,
    selector: String,
    run_id: i64,
    generation: u64,
    // Keep the cross-process scheduler lock alive even if subscription reconciliation replaces
    // the BenchmarkStore while this arbitrary external program is still running.
    process_lease: crate::storage::UsabilityProbeLockLease,
    selector_member_count: usize,
    received: BTreeMap<String, UsabilityProbeNodeResult>,
    stage: String,
    current_node: Option<String>,
    current_node_started_at: Option<Instant>,
    progress: Option<UsabilityProbeProgress>,
    result_ttl: Option<Duration>,
    background: bool,
    quality_persistence_error: Option<String>,
    job: UsabilityProbeJob,
}

impl App {
    pub(super) fn start_manual_usability_probe(&mut self) {
        if self.usability_probe_job.is_some() {
            self.set_status_only("A usability probe is already running");
            return;
        }
        let manifest_id = match &self.node_view_panel {
            NodeViewPanel::Streaming => NodeViewId::streaming(),
            NodeViewPanel::Custom(id) => id.clone(),
            _ => {
                self.set_status_only("Select a usability node-view tab before pressing U");
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
            bail!("a usability probe is already running");
        }
        let execution = match &manifest.source {
            crate::usability_probe::UsabilityProbeSource::Builtin(_) => {
                let (config_path, sing_box_executable) = self
                    .sustained_runtime_environment
                    .clone()
                    .context("managed runtime environment is unavailable")?;
                Some(UsabilityProbeExecutionContext {
                    builtin: BuiltinProbeContext {
                        config_path,
                        sing_box_executable,
                        streaming_prefilter_url: self.benchmark_url.clone(),
                        streaming_target_url: self.sustained_target_url.clone(),
                        connectivity_timeout_ms: self.benchmark_timeout_ms,
                        agy_executable: std::env::var_os(AGY_EXECUTABLE_ENV)
                            .filter(|value| !value.is_empty())
                            .map(std::path::PathBuf::from)
                            .unwrap_or_else(|| std::path::PathBuf::from("agy")),
                    },
                })
            }
            crate::usability_probe::UsabilityProbeSource::Url(_)
            | crate::usability_probe::UsabilityProbeSource::Executable { .. } => None,
        };
        let (run_id, generation, process_lease) = self
            .benchmark_workflow
            .begin_usability_probe_run(manifest.id.as_str(), &group.name)?;
        self.manual_candidate_navigation = false;
        let job = spawn_usability_probe_job_with_context(
            manifest.clone(),
            group.members.clone(),
            self.client.base_url.clone(),
            self.client.client.clone(),
            execution,
        )
        .map_err(|error| {
            let diagnostic = format!("failed to launch usability probe: {error:#}");
            let _ = self.benchmark_workflow.finish_usability_probe_run_with_ttl(
                UsabilityProbeRunFinalization {
                    run_id,
                    generation,
                    process_lease: &process_lease,
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
            process_lease,
            selector_member_count: group.members.len(),
            received: BTreeMap::new(),
            stage: "Starting usability probe...".to_string(),
            current_node: None,
            current_node_started_at: None,
            progress: None,
            result_ttl: manifest.result_ttl,
            background,
            quality_persistence_error: None,
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
        let Some(manifest_id) = self.active_usability_manifest_id() else {
            self.set_status_only("Select a usability node-view tab before pressing P");
            return Ok(());
        };
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
        let schedules = self
            .background_probe_enabled
            .iter()
            .filter_map(|id| {
                let manifest = self
                    .usability_probe_manifests
                    .iter()
                    .find(|manifest| &manifest.id == id && manifest.background)?;
                let selector = self.background_probe_selectors.get(id)?;
                let key = (id.clone(), selector.clone());
                Some((manifest.clone(), selector.clone(), key))
            })
            .collect::<Vec<_>>();
        let now_ms = current_time_ms();
        let mut due = None;
        for (manifest, selector, key) in schedules {
            if !self.last_background_probe_started.contains_key(&key) {
                match self
                    .benchmark_workflow
                    .latest_usability_probe_started_at_ms(manifest.id.as_str(), &selector)
                {
                    Ok(Some(started_at_ms)) => {
                        // WHY: worker restarts lose their monotonic clock. Re-anchor the persisted
                        // wall-clock start once, then continue on Instant so clock adjustments
                        // cannot cause an early repeat of a paid application-level probe.
                        if let Some(restored) = restored_probe_start(
                            now,
                            now_ms,
                            started_at_ms,
                            manifest.background_interval(),
                        ) {
                            self.last_background_probe_started
                                .insert(key.clone(), restored);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.set_status_only(format!(
                            "Scheduled {} probe deferred while restoring its interval: {error:#}",
                            manifest.label
                        ));
                        return;
                    }
                }
            }
            if self
                .last_background_probe_started
                .get(&key)
                .is_none_or(|last| now.duration_since(*last) >= manifest.background_interval())
            {
                due = Some((manifest, selector, key));
                break;
            }
        }
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
                    let quality_persistence = result.sustained_quality.as_ref().map(|quality| {
                        let active = self
                            .usability_probe_job
                            .as_ref()
                            .expect("progress requires an active usability probe");
                        self.benchmark_workflow.record_custom_sustained_quality(
                            &active.selector,
                            active.generation,
                            quality,
                        )
                    });
                    let persistence_diagnostic = match quality_persistence {
                        Some(Err(error)) => Some(format!(
                            "failed to persist Streaming throughput evidence: {error:#}"
                        )),
                        Some(Ok(false)) => {
                            Some("Streaming throughput evidence became stale".to_string())
                        }
                        Some(Ok(true)) | None => None,
                    };
                    if let Some(diagnostic) = persistence_diagnostic {
                        if let Some(active) = self.usability_probe_job.as_mut() {
                            active.quality_persistence_error = Some(diagnostic);
                        }
                    }
                    let (manifest_id, received_count, selector_member_count) = {
                        let Some(active) = self.usability_probe_job.as_mut() else {
                            continue;
                        };
                        active.received.insert(result.node.clone(), result);
                        if active.current_node.as_deref() == Some(latest_result.node.as_str()) {
                            active.current_node = None;
                            active.current_node_started_at = None;
                        }
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
                        .map(|manifest| manifest.label.clone())
                        .unwrap_or_else(|| "Custom".to_string());
                    self.sync_selection_to_displayed_members();
                    self.set_status_only(usability_probe_progress_status(
                        &label,
                        received_count,
                        selector_member_count,
                        &latest_result,
                    ));
                }
                UsabilityProbeJobEvent::Status {
                    message,
                    node,
                    candidate,
                    progress,
                } => {
                    let Some(active) = self.usability_probe_job.as_mut() else {
                        continue;
                    };
                    active.stage = message.clone();
                    let next_node = candidate.then_some(node).flatten();
                    if next_node != active.current_node {
                        active.current_node_started_at = next_node.as_ref().map(|_| Instant::now());
                    }
                    active.current_node = next_node;
                    if progress.is_some() {
                        active.progress = progress;
                    }
                    self.sync_selection_to_displayed_members();
                    self.set_status_only(message);
                }
                UsabilityProbeJobEvent::Finished(mut completion) => {
                    self.manual_candidate_navigation = true;
                    let Some(mut active) = self.usability_probe_job.take() else {
                        continue;
                    };
                    active.job.join();
                    if let Some(diagnostic) = active.quality_persistence_error.take() {
                        completion.complete = false;
                        completion.diagnostic = Some(diagnostic);
                    }
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
                            process_lease: &active.process_lease,
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
                                        format!("; background worker refresh deferred: {error:#}")
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
                        Ok(false) => {
                            // The prior complete rows stay published, but the same cached state
                            // also carries the newer failed-attempt diagnostic shown by panel/detail.
                            self.refresh_cached_usability_run(
                                &active.manifest_id,
                                &active.selector,
                            );
                            self.set_status_only(format!(
                                "{label} probe incomplete; the previous complete result was preserved{}",
                                usability_status_suffix(completion.diagnostic.as_deref())
                            ));
                        }
                        Err(error) => self.set_status_only(format!(
                            "{label} probe could not publish results: {error:#}"
                        )),
                    }
                }
            }
        }
    }

    pub(super) fn cancel_active_usability_probe(&mut self) -> Result<()> {
        self.cancel_active_usability_probe_with_reason("TUI shutdown cancelled the usability probe")
    }

    pub(super) fn cancel_active_usability_probe_with_reason(
        &mut self,
        diagnostic: &str,
    ) -> Result<()> {
        let Some(mut active) = self.usability_probe_job.take() else {
            return Ok(());
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
        let persisted = self.benchmark_workflow.finish_usability_probe_run_with_ttl(
            UsabilityProbeRunFinalization {
                run_id: active.run_id,
                generation: active.generation,
                process_lease: &active.process_lease,
                complete: false,
                summary: None,
                diagnostic: Some(diagnostic),
                facts: &facts,
                result_ttl: active.result_ttl,
            },
        );
        persisted.context("failed to persist cancelled usability probe")?;
        self.refresh_cached_usability_run(&active.manifest_id, &active.selector);
        Ok(())
    }

    pub(super) fn cancel_revoked_background_usability_probe(
        &mut self,
        enabled: &std::collections::BTreeSet<NodeViewId>,
        selectors: &std::collections::BTreeMap<NodeViewId, String>,
    ) -> Result<()> {
        let revoked = self.usability_probe_job.as_ref().is_some_and(|active| {
            active.background
                && (!enabled.contains(&active.manifest_id)
                    || selectors.get(&active.manifest_id) != Some(&active.selector))
        });
        if revoked {
            // User permission is a continuous condition. ApplyConfig revocation must stop an
            // already-running paid probe, not merely prevent the next scheduled launch.
            self.cancel_active_usability_probe_with_reason(
                "background usability permission was revoked while the probe was running",
            )?;
        }
        Ok(())
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
            (!attempt.complete).then(|| {
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
        self.custom_node_view_projection_from_run(
            manifest_id,
            selector,
            selector_members,
            run.as_ref(),
        )
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
        Ok(self.custom_node_view_projection_from_run(
            manifest_id,
            selector,
            selector_members,
            run.as_ref(),
        ))
    }

    fn custom_node_view_projection_from_run(
        &self,
        manifest_id: &NodeViewId,
        selector: &str,
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
                        Some(true)
                            if manifest_id == &NodeViewId::streaming()
                                && self
                                    .benchmark_workflow
                                    .sustained_quality(selector, node)
                                    .and_then(|quality| quality.completed())
                                    .is_none() =>
                        {
                            PanelMembership::Untested
                        }
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

    pub(super) fn is_usability_probe_active_for(
        &self,
        manifest_id: &NodeViewId,
        selector: Option<&str>,
    ) -> bool {
        self.usability_probe_job.as_ref().is_some_and(|active| {
            &active.manifest_id == manifest_id
                && selector.is_none_or(|sel| active.selector == sel)
        })
    }

    pub(super) fn custom_usability_probe_progress(
        &self,
        manifest_id: &NodeViewId,
        selector: &str,
    ) -> Option<(usize, usize, Option<UsabilityProbeProgress>)> {
        self.usability_probe_job.as_ref().and_then(|active| {
            (&active.manifest_id == manifest_id && active.selector == selector).then_some((
                active.received.len(),
                active.selector_member_count,
                active.progress.clone(),
            ))
        })
    }

    pub(super) fn custom_usability_probe_stage(
        &self,
        manifest_id: &NodeViewId,
        selector: &str,
    ) -> Option<String> {
        self.usability_probe_job.as_ref().and_then(|active| {
            (&active.manifest_id == manifest_id && active.selector == selector)
                .then(|| active.stage.clone())
        })
    }

    pub(super) fn custom_usability_live_projection(
        &self,
        manifest_id: &NodeViewId,
        selector: &str,
    ) -> Option<(
        Option<(String, u64, String)>,
        BTreeMap<String, UsabilityProbeNodeResult>,
    )> {
        self.usability_probe_job.as_ref().and_then(|active| {
            (&active.manifest_id == manifest_id && active.selector == selector).then(|| {
                let stage_label = active
                    .progress
                    .as_ref()
                    .map(|p| p.stage_two_label.clone())
                    .unwrap_or_default();
                let pending = active.current_node.clone().map(|node| {
                    let elapsed = active
                        .current_node_started_at
                        .map(|started| started.elapsed().as_secs())
                        .unwrap_or_default();
                    (node, elapsed, stage_label)
                });
                (pending, active.received.clone())
            })
        })
    }
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(super) fn restored_probe_start(
    now: Instant,
    now_ms: u64,
    started_at_ms: u64,
    interval: Duration,
) -> Option<Instant> {
    let elapsed = Duration::from_millis(now_ms.saturating_sub(started_at_ms));
    (elapsed < interval).then(|| now.checked_sub(elapsed).unwrap_or(now))
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
                sustained_quality: None,
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
            visible: true,
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
