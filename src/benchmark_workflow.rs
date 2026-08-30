use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::Client as AsyncClient;
use serde::{Deserialize, Serialize};

use crate::auto_pick::{BackgroundLatencyResult, BackgroundLatencySnapshot};
use crate::automatic_selection::{NodeQualityFacts, ReachabilityTier};
use crate::config::parse_sing_box_config_text;
use crate::config_mutation::lock_config_mutation_for;
use crate::controller::{
    BenchmarkEvent, BenchmarkRequest, BenchmarkResult, BenchmarkSummary,
    NodeReachabilityAssessment, ProbeOutcome, spawn_reachability_assessment_worker,
};
use crate::defaults::SINGLE_NODE_RETEST_DEBOUNCE;
use crate::node_quality_path::{
    canonical_config_target, ensure_active_config_paths_are_distinct, node_quality_reserved_paths,
};
use crate::node_runtime_manager::IsolatedRuntimeSnapshot;
use crate::storage::{
    BenchmarkRecord, BenchmarkStore, NodeLatencySample, NodeQualityReadLease, NodeQuickHistory,
    PersistedNodeQualityProjection, SustainedSuccessStats, lock_node_quality_reconciliation,
};
use crate::sustained_quality::{
    NodeSustainedQuality, SustainedCompletion, SustainedProbeEvent, SustainedProbeRequest,
    spawn_sustained_probe_worker, sustained_target_identity, validate_sustained_target,
};

type NodeMetricKey = (String, String);
type QuickHistoryCache = BTreeMap<NodeMetricKey, NodeQuickHistory>;
type SustainedStatsCache = BTreeMap<NodeMetricKey, SustainedSuccessStats>;
type MetricCaches = (QuickHistoryCache, SustainedStatsCache);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StreamingNodeProjection {
    pub(crate) name: String,
    pub(crate) assessment: NodeReachabilityAssessment,
    pub(crate) completion: SustainedCompletion,
    pub(crate) sustained_stats: SustainedSuccessStats,
    pub(crate) quick_history: NodeQuickHistory,
}

pub(crate) const QUALITY_RUNTIME_RECEIPT_ENV: &str = "SING_BOX_TUI_QUALITY_RUNTIME_RECEIPT";

/// Proof that one observed controller process loaded one exact node-quality generation.
///
/// This is intentionally separate from `SustainedJob::target_identity`: the receipt identifies
/// the managed runtime carrying probe traffic, while the job identity identifies the account-free
/// HTTPS object whose transfer is being measured.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct QualityRuntimeReceipt {
    canonical_config_path: PathBuf,
    canonical_database_path: PathBuf,
    controller_base_url: String,
    quality_generation: u64,
    managed_pid: Option<u32>,
}

impl QualityRuntimeReceipt {
    pub(crate) fn quality_generation(&self) -> u64 {
        self.quality_generation
    }

    pub(crate) fn managed_pid(&self) -> Option<u32> {
        self.managed_pid
    }

    pub(crate) fn canonical_config_path(&self) -> &Path {
        &self.canonical_config_path
    }

    pub(crate) fn encode_for_child(&self) -> Result<String> {
        serde_json::to_string(self).context("failed to encode node-quality runtime receipt")
    }

    pub(crate) fn decode_from_child(encoded: &str) -> Result<Self> {
        serde_json::from_str(encoded).context("failed to decode node-quality runtime receipt")
    }
}

/// Values observed by the managed reload callback before a runtime fence may be cleared.
pub(crate) struct ManagedRuntimeObservation<T> {
    output: T,
    config_path: PathBuf,
    controller_base_url: String,
    managed_pid: Option<u32>,
}

impl<T> ManagedRuntimeObservation<T> {
    pub(crate) fn new(
        output: T,
        config_path: impl Into<PathBuf>,
        controller_base_url: impl Into<String>,
        managed_pid: Option<u32>,
    ) -> Self {
        Self {
            output,
            config_path: config_path.into(),
            controller_base_url: controller_base_url.into(),
            managed_pid,
        }
    }
}

pub(crate) struct BenchmarkWorkflow {
    base_url: String,
    client: AsyncClient,
    summaries: BTreeMap<String, BenchmarkSummary>,
    reachability_assessments: BTreeMap<NodeMetricKey, NodeReachabilityAssessment>,
    sustained_quality: BTreeMap<NodeMetricKey, NodeSustainedQuality>,
    sustained_target_identity: String,
    quick_history_cache: QuickHistoryCache,
    sustained_stats_cache: SustainedStatsCache,
    jobs: Vec<BenchmarkJob>,
    sustained_jobs: Vec<SustainedJob>,
    last_single_node: Option<(String, String, Instant)>,
    store: Option<BenchmarkStore>,
    runtime_receipt: Option<QualityRuntimeReceipt>,
    next_auto_selection_round_id: u64,
    latency_order: bool,
    #[cfg(test)]
    allow_unpersisted_quality_for_test: bool,
    #[cfg(test)]
    after_sustained_persist_hook: Option<Box<dyn FnMut()>>,
    #[cfg(test)]
    skip_active_environment_check_for_test: bool,
    #[cfg(test)]
    background_projection_reload_count: usize,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BenchmarkStart {
    Started,
    AlreadyRunning,
    Debounced,
    NoCandidates,
    CancellationRequested,
}

pub(crate) enum BenchmarkUpdate {
    Progress {
        group: String,
        best_label: String,
    },
    SustainedProgress {
        group: String,
        result: NodeSustainedQuality,
    },
    Finished(BenchmarkCompletion),
    Disconnected {
        group: String,
    },
}

pub(crate) enum BenchmarkCompletion {
    Group {
        group: String,
        assessed: usize,
        assessments: Vec<NodeReachabilityAssessment>,
        quality_current: bool,
    },
    AutoSelect {
        group: String,
        round_id: u64,
        assessments: Vec<NodeReachabilityAssessment>,
        quality_current: bool,
    },
    SingleNode {
        group: String,
        node: String,
        assessment: Option<NodeReachabilityAssessment>,
        quality_current: bool,
    },
    Sustained {
        group: String,
        kind: SustainedKind,
        completed: usize,
        attempted: usize,
        infrastructure_failures: usize,
        cancelled: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SustainedKind {
    Automatic,
    SingleNode,
}

#[derive(Clone)]
enum BenchmarkKind {
    Group,
    AutoSelect,
    SingleNode { node: String },
}

impl BenchmarkKind {
    fn label(&self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::AutoSelect => "auto",
            Self::SingleNode { .. } => "single",
        }
    }
}

struct BenchmarkJob {
    group: String,
    nodes: Vec<String>,
    filter: String,
    kind: BenchmarkKind,
    receiver: mpsc::Receiver<BenchmarkEvent>,
    worker: JoinHandle<()>,
    cancellation: Option<Arc<AtomicBool>>,
    current_assessments: BTreeMap<String, NodeReachabilityAssessment>,
    quality_receipt: Option<QualityRuntimeReceipt>,
    quality_projection_current: bool,
    auto_selection_round_id: Option<u64>,
}

struct SustainedJob {
    group: String,
    outstanding_nodes: BTreeSet<String>,
    target_identity: String,
    quality_generation: u64,
    kind: SustainedKind,
    receiver: mpsc::Receiver<SustainedProbeEvent>,
    worker: JoinHandle<()>,
    cancellation: Arc<AtomicBool>,
    attributable_attempts: usize,
    completed_results: usize,
    infrastructure_failures: usize,
    cancelled_results: usize,
    persistence_failed: bool,
}

impl BenchmarkWorkflow {
    pub(crate) fn open(
        base_url: String,
        client: AsyncClient,
        config_path: &Path,
        database_path: &Path,
        sustained_target_url: &str,
    ) -> Result<Self> {
        Self::open_with_binding_hook(
            base_url,
            client,
            config_path,
            database_path,
            sustained_target_url,
            || {},
        )
    }

    fn open_with_binding_hook<Hook>(
        base_url: String,
        client: AsyncClient,
        config_path: &Path,
        database_path: &Path,
        sustained_target_url: &str,
        after_config_read: Hook,
    ) -> Result<Self>
    where
        Hook: FnOnce(),
    {
        let target_identity = sustained_target_identity(sustained_target_url)?;
        ensure_active_config_paths_are_distinct(config_path, database_path, &[])?;
        let _config_guard = lock_config_mutation_for(config_path)?;
        let config_text = match fs::read_to_string(config_path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Self::new(base_url, client, None, target_identity));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read active sing-box config {} before opening node-quality persistence",
                        config_path.display()
                    )
                });
            }
        };
        let config = parse_sing_box_config_text(&config_text).with_context(|| {
            format!(
                "failed to parse active sing-box config {} before opening node-quality persistence",
                config_path.display()
            )
        })?;
        after_config_read();
        let quality_guard = lock_node_quality_reconciliation(database_path)?;
        let store =
            BenchmarkStore::open_while_reconciliation_locked(database_path, &quality_guard)?;
        store.bind_node_history_while_reconciliation_locked(&quality_guard, &config)?;
        let runtime_reload_required = store.runtime_reload_required()?;
        // `Self::new` loads the persisted projection and takes the quality lock itself.
        drop(quality_guard);
        Ok(Self::new(
            base_url,
            client,
            (!runtime_reload_required).then_some(store),
            target_identity,
        ))
    }

    /// Runs a managed sing-box reload against one locked config snapshot, then re-enables facts.
    ///
    /// The callback must not return success until the controller for the newly started process is
    /// ready. Holding the canonical config lock across both startup and observation is what makes
    /// clearing the durable runtime fence a proof about the config that sing-box actually loaded.
    pub(crate) fn confirm_managed_runtime_reload<T, Reload>(
        &mut self,
        config_path: &Path,
        database_path: &Path,
        reload: Reload,
    ) -> Result<T>
    where
        Reload: FnOnce() -> Result<ManagedRuntimeObservation<T>>,
    {
        ensure_active_config_paths_are_distinct(config_path, database_path, &[])?;
        // A failed or misdirected restart invalidates the old runtime proof even when identities
        // did not change. Disable projections before invoking any process lifecycle operation.
        self.pause_quality_persistence();
        let _config_guard = lock_config_mutation_for(config_path)?;
        let canonical_config_path = canonical_config_target(config_path)?;
        let canonical_database_path = node_quality_reserved_paths(database_path)?
            .into_iter()
            .next()
            .context("node-quality reserved path list is empty")?;
        let config_text = fs::read_to_string(config_path).with_context(|| {
            format!(
                "failed to read active sing-box config {} before managed reload",
                config_path.display()
            )
        })?;
        let config = parse_sing_box_config_text(&config_text).with_context(|| {
            format!(
                "failed to parse active sing-box config {} before managed reload",
                config_path.display()
            )
        })?;
        let quality_guard = lock_node_quality_reconciliation(database_path)?;
        let store =
            BenchmarkStore::open_while_reconciliation_locked(database_path, &quality_guard)?;
        store.bind_node_history_while_reconciliation_locked(&quality_guard, &config)?;
        // Any process restart invalidates the old proof even when outbound identities are byte-for-
        // byte unchanged. Persist the fence before the callback so other TUI/headless processes
        // fail closed throughout a failed, interrupted, or misdirected lifecycle operation.
        store.ensure_runtime_reload_required()?;

        // Do not clear this cross-process fence merely because a TUI restarted. The callback's
        // successful readiness observation, while the exact config remains locked, is the point
        // at which old same-tag controller results become attributable to the bound identities.
        let observation = reload()?;
        let observed_config_path = canonical_config_target(&observation.config_path)?;
        if observed_config_path != canonical_config_path {
            anyhow::bail!(
                "managed reload observed config {}, expected {}",
                observed_config_path.display(),
                canonical_config_path.display()
            );
        }
        let observed_controller = normalize_controller_base_url(&observation.controller_base_url);
        let expected_controller = normalize_controller_base_url(&self.base_url);
        if observed_controller != expected_controller {
            anyhow::bail!(
                "managed reload observed controller {observed_controller}, expected {expected_controller}"
            );
        }
        let managed_pid = observation
            .managed_pid
            .context("managed reload did not report the observed process id")?;
        if managed_pid == 0 {
            anyhow::bail!("managed reload reported invalid pid 0");
        }
        let runtime_receipt = QualityRuntimeReceipt {
            canonical_config_path,
            canonical_database_path,
            controller_base_url: expected_controller,
            quality_generation: store.quality_generation(),
            managed_pid: Some(managed_pid),
        };
        store
            .clear_runtime_reload_required()
            .context("managed sing-box loaded the config but quality persistence stayed fenced")?;
        drop(quality_guard);
        self.install_store(Some(store));
        self.runtime_receipt = Some(runtime_receipt);
        Ok(observation.output)
    }

    /// Attaches a headless worker to a proof created by the foreground managed-runtime startup.
    ///
    /// Unlike managed confirmation, adoption can never clear a reload fence. A config drift,
    /// generation change, controller mismatch, or surviving fence makes the child fail closed.
    pub(crate) fn adopt_runtime_receipt(
        &mut self,
        config_path: &Path,
        database_path: &Path,
        receipt: QualityRuntimeReceipt,
    ) -> Result<()> {
        ensure_active_config_paths_are_distinct(config_path, database_path, &[])?;
        self.pause_quality_persistence();
        let _config_guard = lock_config_mutation_for(config_path)?;
        let canonical_config_path = canonical_config_target(config_path)?;
        let canonical_database_path = node_quality_reserved_paths(database_path)?
            .into_iter()
            .next()
            .context("node-quality reserved path list is empty")?;
        if receipt.canonical_config_path != canonical_config_path
            || receipt.canonical_database_path != canonical_database_path
            || receipt.controller_base_url != normalize_controller_base_url(&self.base_url)
        {
            anyhow::bail!(
                "headless runtime receipt does not match config, database, and controller"
            );
        }
        let managed_pid = receipt
            .managed_pid
            .context("headless runtime receipt does not include a managed process id")?;
        #[cfg(test)]
        let skip_active_environment_check = self.skip_active_environment_check_for_test;
        #[cfg(not(test))]
        let skip_active_environment_check = false;
        if managed_pid == 0
            || (!skip_active_environment_check
                && !crate::node_runtime_manager::active_environment_matches(
                    managed_pid,
                    &canonical_config_path,
                )?)
        {
            anyhow::bail!("headless runtime receipt does not match a verified active process");
        }
        let config_text = fs::read_to_string(config_path).with_context(|| {
            format!(
                "failed to read active sing-box config {} before adopting runtime receipt",
                config_path.display()
            )
        })?;
        let config = parse_sing_box_config_text(&config_text).with_context(|| {
            format!(
                "failed to parse active sing-box config {} before adopting runtime receipt",
                config_path.display()
            )
        })?;
        let quality_guard = lock_node_quality_reconciliation(database_path)?;
        let store =
            BenchmarkStore::open_while_reconciliation_locked(database_path, &quality_guard)?;
        store.bind_node_history_while_reconciliation_locked(&quality_guard, &config)?;
        if store.runtime_reload_required()?
            || store.quality_generation() != receipt.quality_generation
        {
            anyhow::bail!(
                "headless runtime receipt is stale or node-quality reload remains fenced"
            );
        }
        drop(quality_guard);
        self.install_store(Some(store));
        self.runtime_receipt = Some(receipt);
        Ok(())
    }

    pub(crate) fn runtime_receipt(&self) -> Option<&QualityRuntimeReceipt> {
        self.runtime_receipt.as_ref().filter(|receipt| {
            self.store.as_ref().is_some_and(|store| {
                store.quality_generation() == receipt.quality_generation
                    && store.quality_session_current().unwrap_or(false)
                    && receipt.controller_base_url == normalize_controller_base_url(&self.base_url)
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn open_with_binding_hook_for_test<Hook>(
        base_url: String,
        client: AsyncClient,
        config_path: &Path,
        database_path: &Path,
        sustained_target_url: &str,
        after_config_read: Hook,
    ) -> Result<Self>
    where
        Hook: FnOnce(),
    {
        Self::open_with_binding_hook(
            base_url,
            client,
            config_path,
            database_path,
            sustained_target_url,
            after_config_read,
        )
    }

    fn new(
        base_url: String,
        client: AsyncClient,
        store: Option<BenchmarkStore>,
        sustained_target_identity: String,
    ) -> Self {
        let reachability_assessments = store
            .as_ref()
            .and_then(|store| store.latest_reachability_assessments().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(selector, assessment)| ((selector, assessment.name.clone()), assessment))
            .collect();
        let sustained_quality = store
            .as_ref()
            .and_then(|store| {
                store
                    .latest_sustained_quality(&sustained_target_identity)
                    .ok()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|(selector, result)| ((selector, result.name.clone()), result))
            .collect();
        let (quick_history_cache, sustained_stats_cache) = load_metric_caches(
            store.as_ref(),
            &reachability_assessments,
            &sustained_quality,
            &sustained_target_identity,
        );
        Self {
            base_url,
            client,
            summaries: BTreeMap::new(),
            reachability_assessments,
            sustained_quality,
            sustained_target_identity,
            quick_history_cache,
            sustained_stats_cache,
            jobs: Vec::new(),
            sustained_jobs: Vec::new(),
            last_single_node: None,
            store,
            runtime_receipt: None,
            next_auto_selection_round_id: 1,
            latency_order: false,
            #[cfg(test)]
            allow_unpersisted_quality_for_test: false,
            #[cfg(test)]
            after_sustained_persist_hook: None,
            #[cfg(test)]
            skip_active_environment_check_for_test: false,
            #[cfg(test)]
            background_projection_reload_count: 0,
        }
    }

    pub(crate) fn summary(&self, group: &str) -> Option<&BenchmarkSummary> {
        self.summaries.get(group)
    }

    pub(crate) fn reachability_assessment(
        &self,
        group: &str,
        node: &str,
    ) -> Option<&NodeReachabilityAssessment> {
        self.reachability_assessments
            .get(&(group.to_string(), node.to_string()))
    }

    pub(crate) fn sustained_quality(
        &self,
        group: &str,
        node: &str,
    ) -> Option<&NodeSustainedQuality> {
        if !self.quality_session_current() {
            return None;
        }
        self.sustained_quality
            .get(&(group.to_string(), node.to_string()))
    }

    pub(crate) fn sustained_target_identity(&self) -> &str {
        &self.sustained_target_identity
    }

    pub(crate) fn quick_eligible(&self, group: &str, node: &str) -> bool {
        self.reachability_assessment(group, node)
            .is_some_and(assessment_is_quick_eligible)
    }

    pub(crate) fn latency_order(&self) -> bool {
        self.latency_order
    }

    pub(crate) fn toggle_latency_order(&mut self) -> bool {
        self.latency_order = !self.latency_order;
        self.latency_order
    }

    pub(crate) fn acquire_quality_read_lease(&self) -> Result<NodeQualityReadLease> {
        #[cfg(test)]
        if self.allow_unpersisted_quality_for_test && self.store.is_none() {
            return Ok(NodeQualityReadLease::for_test(0));
        }
        self.store
            .as_ref()
            .context("node-quality persistence is not active")?
            .acquire_quality_read_lease()?
            .context("node-quality identity generation changed; rerun the assessment")
    }

    pub(crate) fn acquire_auto_selection_read_lease(&self) -> Result<NodeQualityReadLease> {
        #[cfg(test)]
        if self.allow_unpersisted_quality_for_test && self.store.is_none() {
            return Ok(NodeQualityReadLease::for_test(0));
        }
        let receipt = self
            .runtime_receipt()
            .context("automatic selection requires a confirmed managed runtime receipt")?;
        let lease = self.acquire_quality_read_lease()?;
        // The generic lease proves store generation only. A selector write additionally requires
        // the receipt for the exact managed controller runtime that will receive that write.
        anyhow::ensure!(
            lease.generation() == receipt.quality_generation,
            "node-quality runtime receipt generation is stale"
        );
        Ok(lease)
    }

    pub(crate) fn node_quality_facts_with_lease(
        &self,
        lease: &NodeQualityReadLease,
        group: &str,
        members: &[String],
    ) -> Result<Vec<NodeQualityFacts>> {
        match self.store.as_ref() {
            Some(store) => store.validate_quality_read_lease(lease)?,
            #[cfg(test)]
            None if self.allow_unpersisted_quality_for_test => {}
            None => anyhow::bail!("node-quality persistence is not active"),
        }

        Ok(members
            .iter()
            .enumerate()
            .map(|(config_order, node)| {
                let key = (group.to_string(), node.clone());
                let assessment = self.reachability_assessments.get(&key);
                let reachability = assessment.and_then(|assessment| {
                    assessment.assessment.map(|_| {
                        ReachabilityTier::from_successes(
                            assessment
                                .attempts
                                .iter()
                                .filter(|outcome| matches!(outcome, ProbeOutcome::Reachable { .. }))
                                .count() as u8,
                        )
                    })
                });
                let quick = self.quick_history(group, node);
                let sustained = self.sustained_quality.get(&key);
                let sustained_stats = self
                    .sustained_stats_cache
                    .get(&key)
                    .copied()
                    .unwrap_or_default();
                NodeQualityFacts {
                    node: node.clone(),
                    reachability,
                    recent_quick_successes: quick.successful_rounds,
                    recent_quick_rounds: quick.rounds,
                    warm_median_ms: quick.warm_median_ms,
                    p95_ms: quick.p95_ms,
                    cold_start_ms: quick.cold_start_ms,
                    sustained_successes: sustained_stats.successes,
                    sustained_attempts: sustained_stats.attempts,
                    throughput_bytes_per_second: sustained
                        .and_then(NodeSustainedQuality::completed)
                        .map(|completion| completion.throughput_bytes_per_second),
                    config_order,
                }
            })
            .collect())
    }

    pub(crate) fn activate_sustained_target(&mut self, target_url: &str) -> Result<()> {
        let identity = sustained_target_identity(target_url)?;
        if identity == self.sustained_target_identity {
            return Ok(());
        }
        let sustained_quality = match self.store.as_ref() {
            Some(store) => store.latest_sustained_quality(&identity)?,
            None => Vec::new(),
        }
        .into_iter()
        .map(|(selector, result)| ((selector, result.name.clone()), result))
        .collect::<BTreeMap<_, _>>();
        let mut sustained_stats = BTreeMap::new();
        if let Some(store) = self.store.as_ref() {
            let keys = self
                .reachability_assessments
                .keys()
                .chain(sustained_quality.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for (group, node) in keys {
                sustained_stats.insert(
                    (group.clone(), node.clone()),
                    store.sustained_success_stats(&group, &node, &identity, 10)?,
                );
            }
        }

        // Build the new target projection completely before changing identity or cancelling work.
        // A failed SQLite read therefore leaves the prior target/config generation intact.
        for job in &self.sustained_jobs {
            job.cancellation.store(true, Ordering::Relaxed);
        }
        self.sustained_target_identity = identity;
        self.sustained_quality = sustained_quality;
        self.sustained_stats_cache = sustained_stats;
        Ok(())
    }

    pub(crate) fn start_group(&mut self, request: BenchmarkRequest) -> BenchmarkStart {
        self.start_group_kind(request, BenchmarkKind::Group)
    }

    pub(crate) fn start_auto_select(&mut self, request: BenchmarkRequest) -> BenchmarkStart {
        self.start_group_kind(request, BenchmarkKind::AutoSelect)
    }

    pub(crate) fn start_single_node(
        &mut self,
        request: BenchmarkRequest,
        node: String,
    ) -> BenchmarkStart {
        let group = request.selector.clone();
        if let Some(job) = self
            .jobs
            .iter()
            .find(|job| job.group == group && job.nodes.iter().any(|candidate| candidate == &node))
        {
            if let Some(cancellation) = &job.cancellation {
                cancellation.store(true, Ordering::Relaxed);
                return BenchmarkStart::CancellationRequested;
            }
            return BenchmarkStart::AlreadyRunning;
        }
        if self
            .last_single_node
            .as_ref()
            .is_some_and(|(last_group, last_node, last_started)| {
                last_group == &group
                    && last_node == &node
                    && last_started.elapsed() < SINGLE_NODE_RETEST_DEBOUNCE
            })
        {
            return BenchmarkStart::Debounced;
        }
        self.prepare_single_node(&request, &node);
        self.spawn(request, BenchmarkKind::SingleNode { node: node.clone() });
        self.last_single_node = Some((group, node, Instant::now()));
        BenchmarkStart::Started
    }

    pub(crate) fn start_sustained(
        &mut self,
        mut request: SustainedProbeRequest,
        kind: SustainedKind,
    ) -> Result<BenchmarkStart> {
        validate_sustained_target(&request.target_url)?;
        self.activate_sustained_target(&request.target_url)?;
        let target_identity = self.sustained_target_identity.clone();
        // Keep the established config -> quality lock order while freezing membership and the
        // complete runtime input used by every worker in this job.
        let _config_guard = self
            .store
            .as_ref()
            .map(|_| lock_config_mutation_for(&request.config_path))
            .transpose()?;
        let quality_lease = match self.store.as_ref() {
            Some(store) => {
                let receipt = self
                    .runtime_receipt()
                    .context("sustained probing requires a confirmed managed runtime receipt")?;
                let requested_config = canonical_config_target(&request.config_path)?;
                if requested_config != receipt.canonical_config_path {
                    anyhow::bail!(
                        "sustained probe config {} does not match confirmed runtime {}",
                        requested_config.display(),
                        receipt.canonical_config_path.display()
                    );
                }
                let lease = store
                    .acquire_quality_read_lease()?
                    .context("sustained probing is blocked until node-quality state is rebound")?;
                if lease.generation() != receipt.quality_generation {
                    anyhow::bail!("sustained runtime receipt generation is stale");
                }
                store.retain_bound_node_tags(&lease, &mut request.nodes)?;
                lease
            }
            #[cfg(test)]
            None if self.allow_unpersisted_quality_for_test => NodeQualityReadLease::for_test(0),
            None => anyhow::bail!("sustained probing requires an active node-quality session"),
        };
        let quality_generation = quality_lease.generation();
        if request.nodes.is_empty() {
            return Ok(BenchmarkStart::NoCandidates);
        }
        let overlapping_job = self.sustained_jobs.iter().find(|job| {
            job.group == request.selector
                && job.target_identity == target_identity
                && !job.cancellation.load(Ordering::Relaxed)
                && job
                    .outstanding_nodes
                    .iter()
                    .any(|node| request.nodes.contains(node))
        });
        if kind == SustainedKind::SingleNode {
            if overlapping_job.is_some() {
                // A node-attributable sustained result is identical whether it was requested by
                // `t` or by the bounded automatic follow-on. Reuse the in-flight transfer instead
                // of cancelling its whole batch: cancelling here could discard the requested node
                // before it reports and would also abort unrelated automatic candidates.
                return Ok(BenchmarkStart::AlreadyRunning);
            }
        } else {
            let in_flight = self
                .sustained_jobs
                .iter()
                .filter(|job| {
                    job.group == request.selector
                        && job.target_identity == target_identity
                        && !job.cancellation.load(Ordering::Relaxed)
                })
                .flat_map(|job| job.outstanding_nodes.iter().cloned())
                .collect::<BTreeSet<_>>();
            request.nodes.retain(|node| !in_flight.contains(node));
            if request.nodes.is_empty() {
                return Ok(BenchmarkStart::AlreadyRunning);
            }
        }
        let group = request.selector.clone();
        let nodes = request.nodes.clone();
        let runtime_snapshot = if self.store.is_some() {
            Some(IsolatedRuntimeSnapshot::capture(
                &request.config_path,
                &request.sing_box_executable,
            )?)
        } else {
            #[cfg(not(test))]
            unreachable!("production sustained jobs always have an active store");
            #[cfg(test)]
            None
        };
        let cancellation = Arc::new(AtomicBool::new(false));
        let (tx, receiver) = mpsc::channel();
        let worker =
            spawn_sustained_probe_worker(request, runtime_snapshot, tx, Arc::clone(&cancellation));
        self.sustained_jobs.push(SustainedJob {
            group,
            outstanding_nodes: nodes.iter().cloned().collect(),
            target_identity,
            quality_generation,
            kind,
            receiver,
            worker,
            cancellation,
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });
        Ok(BenchmarkStart::Started)
    }

    pub(crate) fn streaming_members(&self, group: &str, members: &[String]) -> Vec<String> {
        self.streaming_projection(group, members)
            .into_iter()
            .map(|projection| projection.name)
            .collect()
    }

    pub(crate) fn streaming_projection(
        &self,
        group: &str,
        members: &[String],
    ) -> Vec<StreamingNodeProjection> {
        // Copy every field for the rendered rows while one cross-process lease blocks node
        // reconciliation. The owned projection remains coherent even if its generation changes
        // immediately after this method returns and the TUI has not rendered it yet.
        let Ok(_quality_lease) = self.acquire_quality_read_lease() else {
            return Vec::new();
        };
        let mut ranked = members
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                let key = (group.to_string(), node.to_string());
                let assessment = self.reachability_assessments.get(&key)?;
                if !assessment_is_quick_eligible(assessment) {
                    return None;
                }
                let sustained_quality = self.sustained_quality.get(&key)?;
                let completion = sustained_quality.completed()?;
                let sustained = self.sustained_stats_current(group, node, sustained_quality);
                let quick = self.quick_history(group, node);
                Some((
                    StreamingNodeProjection {
                        name: node.clone(),
                        assessment: assessment.clone(),
                        completion: completion.clone(),
                        sustained_stats: sustained,
                        quick_history: quick,
                    },
                    index,
                ))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .0
                .completion
                .throughput_bytes_per_second
                .cmp(&left.0.completion.throughput_bytes_per_second)
                .then_with(|| {
                    compare_ratio_desc(
                        left.0.sustained_stats.successes,
                        left.0.sustained_stats.attempts,
                        right.0.sustained_stats.successes,
                        right.0.sustained_stats.attempts,
                    )
                })
                .then_with(|| {
                    compare_optional_ascending(
                        left.0.quick_history.p95_ms,
                        right.0.quick_history.p95_ms,
                    )
                })
                .then_with(|| {
                    compare_optional_ascending(
                        left.0.quick_history.cold_start_ms,
                        right.0.quick_history.cold_start_ms,
                    )
                })
                .then_with(|| left.1.cmp(&right.1))
        });
        ranked.into_iter().map(|entry| entry.0).collect()
    }

    pub(crate) fn automatic_sustained_candidates(
        &self,
        group: &str,
        current: Option<&str>,
        members: &[String],
        current_assessments: &[NodeReachabilityAssessment],
    ) -> Vec<String> {
        let current_assessments = current_assessments
            .iter()
            .map(|assessment| (assessment.name.as_str(), assessment))
            .collect::<BTreeMap<_, _>>();
        let mut eligible = members
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                let assessment = current_assessments.get(node.as_str()).copied()?;
                if !assessment_is_quick_eligible(assessment) {
                    return None;
                }
                let reachable = assessment
                    .attempts
                    .iter()
                    .filter(|attempt| {
                        matches!(attempt, crate::controller::ProbeOutcome::Reachable { .. })
                    })
                    .count();
                Some((
                    node.clone(),
                    index,
                    reachable,
                    self.quick_history(group, node),
                ))
            })
            .collect::<Vec<_>>();
        eligible.sort_by(|left, right| {
            right
                .2
                .cmp(&left.2)
                .then_with(|| {
                    compare_ratio_desc(
                        left.3.successful_rounds,
                        left.3.rounds,
                        right.3.successful_rounds,
                        right.3.rounds,
                    )
                })
                .then_with(|| {
                    compare_optional_ascending(left.3.warm_median_ms, right.3.warm_median_ms)
                })
                .then_with(|| left.1.cmp(&right.1))
        });

        let mut out = Vec::with_capacity(6);
        if let Some(current) = current.filter(|node| members.iter().any(|item| item == node)) {
            out.push(current.to_string());
        }
        out.extend(
            eligible
                .into_iter()
                .map(|entry| entry.0)
                .filter(|node| Some(node.as_str()) != current)
                .take(5),
        );
        out
    }

    fn sustained_stats_current(
        &self,
        group: &str,
        node: &str,
        sustained_quality: &NodeSustainedQuality,
    ) -> SustainedSuccessStats {
        self.sustained_stats_cache
            .get(&(group.to_string(), node.to_string()))
            .copied()
            // The caller already read this value under its quality lease. Falling back through
            // `sustained_quality()` would try to acquire the same reconciliation lock again.
            .unwrap_or_else(|| SustainedSuccessStats {
                successes: usize::from(sustained_quality.completed().is_some()),
                attempts: 1,
            })
    }

    pub(crate) fn quick_history(&self, group: &str, node: &str) -> NodeQuickHistory {
        let persisted = self
            .quick_history_cache
            .get(&(group.to_string(), node.to_string()))
            .copied()
            .unwrap_or_default();
        if persisted.rounds > 0 {
            return persisted;
        }
        let Some(assessment) = self.reachability_assessment(group, node) else {
            return persisted;
        };
        let mut delays = assessment
            .attempts
            .iter()
            .filter_map(|attempt| match attempt {
                crate::controller::ProbeOutcome::Reachable { delay_ms } => Some(*delay_ms),
                _ => None,
            })
            .collect::<Vec<_>>();
        delays.sort_unstable();
        NodeQuickHistory {
            successful_rounds: usize::from(self.quick_eligible(group, node)),
            rounds: usize::from(assessment.assessment.is_some()),
            warm_median_ms: assessment_warm_median_ms(assessment),
            p95_ms: delays.last().copied(),
            cold_start_ms: assessment
                .attempts
                .first()
                .and_then(|attempt| match attempt {
                    crate::controller::ProbeOutcome::Reachable { delay_ms } => Some(*delay_ms),
                    _ => None,
                }),
        }
    }

    pub(crate) fn poll(&mut self) -> Vec<BenchmarkUpdate> {
        let mut updates = Vec::new();
        let mut finished_indexes = Vec::new();

        for index in 0..self.jobs.len() {
            let mut finished = false;
            loop {
                match self.jobs[index].receiver.try_recv() {
                    Ok(BenchmarkEvent::ReachabilityProgress(assessment)) => {
                        let group = self.jobs[index].group.clone();
                        let quality_receipt = self.jobs[index].quality_receipt.clone();
                        let persisted = self.jobs[index].quality_projection_current
                            && self
                                .record_reachability_assessment(
                                    quality_receipt.as_ref(),
                                    &group,
                                    &assessment,
                                )
                                .unwrap_or_else(|error| {
                                    eprintln!(
                                        "warning: failed to record reachability assessment for {}: {error:#}",
                                        assessment.name
                                    );
                                    false
                                });
                        let best_label = if persisted {
                            let result = BenchmarkResult {
                                name: assessment.name.clone(),
                                delay: assessment_warm_median_ms(&assessment),
                                completed: assessment.assessment.is_some(),
                            };
                            if let Some(summary) = self.summaries.get_mut(&group) {
                                summary.update_result(result.clone());
                            }
                            self.record_result(
                                &group,
                                &self.jobs[index].filter,
                                &self.jobs[index].kind,
                                &result,
                            );
                            assessment.compact_evidence()
                        } else {
                            self.jobs[index].quality_projection_current = false;
                            self.jobs[index].current_assessments.clear();
                            "quality runtime changed; result discarded".to_string()
                        };
                        if persisted {
                            self.jobs[index]
                                .current_assessments
                                .insert(assessment.name.clone(), assessment.clone());
                        }
                        updates.push(BenchmarkUpdate::Progress { group, best_label });
                    }
                    Ok(BenchmarkEvent::Finished) => {
                        finished = true;
                        let group = self.jobs[index].group.clone();
                        let projection_lease = self.quick_projection_lease(index).unwrap_or_else(
                            |error| {
                                eprintln!(
                                    "warning: failed to validate completed reachability assessment: {error:#}"
                                );
                                None
                            },
                        );
                        let quality_current = projection_lease.is_some();
                        if quality_current {
                            let lease = projection_lease
                                .as_ref()
                                .expect("current quick projection owns a read lease");
                            // Persisting each result is only half of the proof: reconciliation may
                            // commit immediately afterward. Publish the batch while the same-runtime
                            // read lease still prevents its generation from being replaced.
                            let assessments = self.jobs[index]
                                .current_assessments
                                .values()
                                .cloned()
                                .collect::<Vec<_>>();
                            let nodes = self.jobs[index].nodes.clone();
                            for assessment in assessments {
                                self.reachability_assessments
                                    .insert((group.clone(), assessment.name.clone()), assessment);
                            }
                            for node in nodes {
                                self.refresh_quick_history_cache_with_lease(lease, &group, &node);
                            }
                        } else {
                            self.jobs[index].quality_projection_current = false;
                            self.jobs[index].current_assessments.clear();
                        }
                        if let Some(completion) = self.completion(index, quality_current) {
                            updates.push(BenchmarkUpdate::Finished(completion));
                        }
                        drop(projection_lease);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        updates.push(BenchmarkUpdate::Disconnected {
                            group: self.jobs[index].group.clone(),
                        });
                        break;
                    }
                }
            }
            if finished {
                finished_indexes.push(index);
            }
        }

        for index in finished_indexes.into_iter().rev() {
            let job = self.jobs.swap_remove(index);
            let _ = job.worker.join();
        }

        let mut finished_sustained = Vec::new();
        for index in 0..self.sustained_jobs.len() {
            let session_current = self.quality_session_current()
                && self.quality_generation_matches(self.sustained_jobs[index].quality_generation);
            if !session_current {
                self.sustained_jobs[index].persistence_failed = true;
                self.sustained_jobs[index]
                    .cancellation
                    .store(true, Ordering::Relaxed);
            }
            let mut finished = false;
            loop {
                match self.sustained_jobs[index].receiver.try_recv() {
                    Ok(SustainedProbeEvent::Progress(result)) => {
                        let group = self.sustained_jobs[index].group.clone();
                        let target_identity = self.sustained_jobs[index].target_identity.clone();
                        let active_target = target_identity == self.sustained_target_identity;
                        self.sustained_jobs[index]
                            .outstanding_nodes
                            .remove(&result.name);
                        let persisted = if session_current {
                            match self.record_sustained_quality(&group, &target_identity, &result) {
                                Ok(persisted) => persisted,
                                Err(error) => {
                                    eprintln!("warning: {error:#}");
                                    false
                                }
                            }
                        } else {
                            false
                        };
                        if !persisted {
                            self.sustained_jobs[index].persistence_failed = true;
                            self.sustained_jobs[index]
                                .cancellation
                                .store(true, Ordering::Relaxed);
                            continue;
                        }
                        #[cfg(test)]
                        if let Some(hook) = self.after_sustained_persist_hook.as_mut() {
                            hook();
                        }
                        // The INSERT's generation gate is not enough: reconciliation can commit
                        // immediately after it. Reacquire the cross-process read lease and keep it
                        // alive through every in-memory projection and emitted progress event.
                        let projection_lease = match self.acquire_quality_read_lease() {
                            Ok(lease)
                                if lease.generation()
                                    == self.sustained_jobs[index].quality_generation =>
                            {
                                lease
                            }
                            Ok(_) | Err(_) => {
                                self.sustained_jobs[index].persistence_failed = true;
                                self.sustained_jobs[index]
                                    .cancellation
                                    .store(true, Ordering::Relaxed);
                                continue;
                            }
                        };
                        let refreshed_stats = self.store.as_ref().and_then(|store| {
                            store
                                .sustained_success_stats_with_lease(
                                    &projection_lease,
                                    &group,
                                    &result.name,
                                    &target_identity,
                                    10,
                                )
                                .ok()
                        });
                        match &result.outcome {
                            crate::sustained_quality::SustainedProbeOutcome::Completed(_) => {
                                self.sustained_jobs[index].attributable_attempts += 1;
                                self.sustained_jobs[index].completed_results += 1;
                            }
                            crate::sustained_quality::SustainedProbeOutcome::TransferFailed {
                                ..
                            } => {
                                self.sustained_jobs[index].attributable_attempts += 1;
                            }
                            crate::sustained_quality::SustainedProbeOutcome::RuntimeFailed {
                                ..
                            } => {
                                self.sustained_jobs[index].infrastructure_failures += 1;
                            }
                            crate::sustained_quality::SustainedProbeOutcome::Cancelled => {
                                self.sustained_jobs[index].cancelled_results += 1;
                            }
                        }
                        if active_target && result.outcome.is_node_attributable() {
                            self.sustained_quality
                                .insert((group.clone(), result.name.clone()), result.clone());
                        }
                        if active_target {
                            if let Some(stats) = refreshed_stats {
                                self.sustained_stats_cache
                                    .insert((group.clone(), result.name.clone()), stats);
                            }
                            updates.push(BenchmarkUpdate::SustainedProgress { group, result });
                        }
                        drop(projection_lease);
                    }
                    Ok(SustainedProbeEvent::Finished) => {
                        finished = true;
                        let job = &self.sustained_jobs[index];
                        let completion_lease = self.acquire_quality_read_lease().ok();
                        if !job.persistence_failed
                            && job.target_identity == self.sustained_target_identity
                            && completion_lease
                                .as_ref()
                                .is_some_and(|lease| lease.generation() == job.quality_generation)
                        {
                            updates.push(BenchmarkUpdate::Finished(
                                BenchmarkCompletion::Sustained {
                                    group: job.group.clone(),
                                    kind: job.kind,
                                    completed: job.completed_results,
                                    attempted: job.attributable_attempts,
                                    infrastructure_failures: job.infrastructure_failures,
                                    cancelled: job.cancelled_results,
                                },
                            ));
                        }
                        drop(completion_lease);
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        if !self.sustained_jobs[index].persistence_failed
                            && self.sustained_jobs[index].target_identity
                                == self.sustained_target_identity
                        {
                            updates.push(BenchmarkUpdate::Disconnected {
                                group: self.sustained_jobs[index].group.clone(),
                            });
                        }
                        break;
                    }
                }
            }
            if finished {
                finished_sustained.push(index);
            }
        }
        for index in finished_sustained.into_iter().rev() {
            let job = self.sustained_jobs.swap_remove(index);
            let _ = job.worker.join();
        }
        updates
    }

    pub(crate) fn apply_background_snapshot(
        &mut self,
        latency: &BackgroundLatencySnapshot,
        active_filter: &str,
    ) -> Result<bool> {
        let generation_matches = self
            .runtime_receipt()
            .is_some_and(|receipt| receipt.quality_generation == latency.quality_generation);
        #[cfg(test)]
        let generation_matches = generation_matches
            || (self.allow_unpersisted_quality_for_test
                && self.store.is_none()
                && latency.quality_generation == 0);
        anyhow::ensure!(
            generation_matches,
            "background node-quality snapshot generation {} is not current",
            latency.quality_generation
        );
        if latency.pattern != active_filter
            || self.jobs.iter().any(|job| {
                job.group == latency.selector && !matches!(job.kind, BenchmarkKind::AutoSelect)
            })
        {
            return Ok(false);
        }

        let mut summary = self
            .summaries
            .get(&latency.selector)
            .cloned()
            .unwrap_or_else(|| BenchmarkSummary::empty(latency.selector.clone()));
        summary.current = latency.current.clone();
        summary.pattern = latency.pattern.clone();
        summary.url = latency.url.clone();
        summary.timeout_ms = latency.timeout_ms;
        summary.max_concurrency = latency.max_concurrency;
        for result in &latency.results {
            summary.update_result(BenchmarkResult {
                name: result.name.clone(),
                delay: result.delay,
                completed: result.completed,
            });
        }

        // The status protocol is only a bounded notification channel, not a second fact store. On
        // an accepted same-generation snapshot, reload when shared SQLite data_version advances so
        // foreground panels/details cannot diverge from the worker's persisted evidence.
        self.reload_persisted_quality_projection(latency.quality_generation)?;
        self.summaries.insert(latency.selector.clone(), summary);
        Ok(true)
    }

    fn reload_persisted_quality_projection(&mut self, expected_generation: u64) -> Result<bool> {
        #[cfg(test)]
        if self.allow_unpersisted_quality_for_test && self.store.is_none() {
            return Ok(false);
        }
        let receipt = self
            .runtime_receipt()
            .context("background fact refresh requires a confirmed managed runtime receipt")?;
        anyhow::ensure!(
            receipt.quality_generation == expected_generation,
            "background fact refresh receipt generation changed"
        );
        let Some(observed_data_version) = self
            .store
            .as_ref()
            .context("background fact refresh requires node-quality persistence")?
            .changed_data_version()?
        else {
            return Ok(false);
        };
        let lease = self.acquire_quality_read_lease()?;
        anyhow::ensure!(
            lease.generation() == expected_generation,
            "background fact refresh lease generation changed"
        );
        let projection = self
            .store
            .as_ref()
            .expect("store checked above")
            .node_quality_projection_with_lease(&lease, &self.sustained_target_identity, 10)?;
        let PersistedNodeQualityProjection {
            reachability_assessments,
            sustained_quality,
            quick_history,
            sustained_stats,
        } = projection;
        let reachability_assessments = reachability_assessments
            .into_iter()
            .map(|(selector, assessment)| ((selector, assessment.name.clone()), assessment))
            .collect();
        let sustained_quality = sustained_quality
            .into_iter()
            .map(|(selector, quality)| ((selector, quality.name.clone()), quality))
            .collect();
        // All owned maps are complete before this mutation point. Any query/generation failure
        // above therefore leaves the prior coherent foreground projection untouched.
        self.reachability_assessments = reachability_assessments;
        self.sustained_quality = sustained_quality;
        self.quick_history_cache = quick_history;
        self.sustained_stats_cache = sustained_stats;
        // Record exactly the pre-read version. If another connection committed after our snapshot,
        // the newer version remains detectable on the next poll instead of being skipped.
        self.store
            .as_ref()
            .expect("store remains installed during foreground refresh")
            .mark_data_version_observed(observed_data_version);
        #[cfg(test)]
        {
            self.background_projection_reload_count += 1;
        }
        drop(lease);
        Ok(true)
    }

    pub(crate) fn background_snapshot(&self, group: &str) -> Option<BackgroundLatencySnapshot> {
        let quality_generation = match self.runtime_receipt() {
            Some(receipt) => receipt.quality_generation,
            #[cfg(test)]
            None if self.allow_unpersisted_quality_for_test && self.store.is_none() => 0,
            None => return None,
        };
        let summary = self.summaries.get(group)?;
        Some(BackgroundLatencySnapshot {
            quality_generation,
            selector: summary.selector.clone(),
            current: summary.current.clone(),
            pattern: summary.pattern.clone(),
            url: summary.url.clone(),
            timeout_ms: summary.timeout_ms,
            max_concurrency: summary.max_concurrency,
            results: summary
                .results
                .iter()
                .map(|result| BackgroundLatencyResult {
                    name: result.name.clone(),
                    delay: result.delay,
                    completed: result.completed,
                })
                .collect(),
        })
    }

    pub(crate) fn node_latency_history(
        &self,
        selector: &str,
        node: &str,
        limit: usize,
    ) -> Result<Option<Vec<NodeLatencySample>>> {
        self.store
            .as_ref()
            .map(|store| store.node_latency_history(selector, node, limit))
            .transpose()
    }

    fn start_group_kind(
        &mut self,
        request: BenchmarkRequest,
        kind: BenchmarkKind,
    ) -> BenchmarkStart {
        if let Some(job) = self.jobs.iter().find(|job| job.group == request.selector) {
            if !matches!(kind, BenchmarkKind::AutoSelect)
                && let Some(cancellation) = &job.cancellation
            {
                cancellation.store(true, Ordering::Relaxed);
                return BenchmarkStart::CancellationRequested;
            }
            return BenchmarkStart::AlreadyRunning;
        }
        if request.nodes.as_ref().is_none_or(Vec::is_empty) {
            return BenchmarkStart::NoCandidates;
        }
        self.prepare_group(&request);
        self.spawn(request, kind);
        BenchmarkStart::Started
    }

    fn prepare_group(&mut self, request: &BenchmarkRequest) {
        let summary = self
            .summaries
            .entry(request.selector.clone())
            .or_insert_with(|| BenchmarkSummary::empty(request.selector.clone()));
        summary.selector = request.selector.clone();
        summary.pattern = request.pattern.clone();
        summary.url = request.url.clone();
        summary.timeout_ms = request.timeout_ms;
        summary.max_concurrency = request.max_concurrency.max(1);
        summary.reset_pending(request.nodes.clone().unwrap_or_default());
    }

    fn prepare_single_node(&mut self, request: &BenchmarkRequest, node: &str) {
        let summary = self
            .summaries
            .entry(request.selector.clone())
            .or_insert_with(|| BenchmarkSummary::empty(request.selector.clone()));
        summary.selector = request.selector.clone();
        summary.pattern = request.pattern.clone();
        summary.url = request.url.clone();
        summary.timeout_ms = request.timeout_ms;
        summary.max_concurrency = 1;
        summary.upsert_pending(node.to_string());
    }

    fn spawn(&mut self, request: BenchmarkRequest, kind: BenchmarkKind) {
        let group = request.selector.clone();
        let nodes = request.nodes.clone().unwrap_or_default();
        let filter = request.pattern.clone();
        let (quality_receipt, quality_projection_current) = self.quality_job_scope();
        let auto_selection_round_id = matches!(&kind, BenchmarkKind::AutoSelect).then(|| {
            let round_id = self.next_auto_selection_round_id;
            self.next_auto_selection_round_id = self.next_auto_selection_round_id.saturating_add(1);
            round_id
        });
        let (tx, receiver) = mpsc::channel();
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker = spawn_reachability_assessment_worker(
            self.base_url.clone(),
            self.client.clone(),
            request,
            tx,
            cancellation.clone(),
        );
        self.jobs.push(BenchmarkJob {
            group,
            nodes,
            filter,
            kind,
            receiver,
            worker,
            cancellation: Some(cancellation),
            current_assessments: BTreeMap::new(),
            quality_receipt,
            quality_projection_current,
            auto_selection_round_id,
        });
    }

    fn completion(&self, job_index: usize, quality_current: bool) -> Option<BenchmarkCompletion> {
        let job = self.jobs.get(job_index)?;
        let group = job.group.clone();
        Some(match &job.kind {
            BenchmarkKind::Group => BenchmarkCompletion::Group {
                assessments: job.current_assessments.values().cloned().collect(),
                assessed: job.current_assessments.len(),
                group,
                quality_current,
            },
            BenchmarkKind::AutoSelect => BenchmarkCompletion::AutoSelect {
                round_id: job.auto_selection_round_id?,
                assessments: job.current_assessments.values().cloned().collect(),
                group,
                quality_current,
            },
            BenchmarkKind::SingleNode { node } => BenchmarkCompletion::SingleNode {
                assessment: job.current_assessments.get(node).cloned(),
                group,
                node: node.clone(),
                quality_current,
            },
        })
    }

    fn record_result(
        &self,
        group: &str,
        filter: &str,
        kind: &BenchmarkKind,
        result: &BenchmarkResult,
    ) {
        let Some(store) = &self.store else {
            return;
        };
        store
            .record_benchmark(&BenchmarkRecord {
                selector: group,
                node: &result.name,
                filter,
                delay_ms: result.delay,
                completed: result.completed,
                job_kind: kind.label(),
            })
            .map(|_| ())
            .with_context(|| format!("failed to record benchmark result for {}", result.name))
            .unwrap_or_else(|error| eprintln!("warning: {error:#}"));
    }

    fn quality_job_scope(&self) -> (Option<QualityRuntimeReceipt>, bool) {
        let receipt = self.runtime_receipt().cloned();
        #[cfg(test)]
        let current =
            receipt.is_some() || (self.allow_unpersisted_quality_for_test && self.store.is_none());
        #[cfg(not(test))]
        let current = receipt.is_some();
        (receipt, current)
    }

    fn record_reachability_assessment(
        &self,
        expected_receipt: Option<&QualityRuntimeReceipt>,
        group: &str,
        assessment: &NodeReachabilityAssessment,
    ) -> Result<bool> {
        #[cfg(test)]
        if self.allow_unpersisted_quality_for_test && self.store.is_none() {
            return Ok(true);
        }
        let Some(expected_receipt) = expected_receipt else {
            return Ok(false);
        };
        if self.runtime_receipt() != Some(expected_receipt) {
            return Ok(false);
        }
        let Some(store) = &self.store else {
            return Ok(false);
        };
        store
            .record_reachability_assessment(group, assessment)
            .with_context(|| {
                format!(
                    "failed to record reachability assessment for {}",
                    assessment.name
                )
            })
    }

    fn quick_projection_lease(&self, job_index: usize) -> Result<Option<NodeQualityReadLease>> {
        let job = &self.jobs[job_index];
        if !job.quality_projection_current {
            return Ok(None);
        }
        #[cfg(test)]
        if self.allow_unpersisted_quality_for_test && self.store.is_none() {
            return Ok(Some(NodeQualityReadLease::for_test(0)));
        }
        let Some(expected_receipt) = job.quality_receipt.as_ref() else {
            return Ok(None);
        };
        if self.runtime_receipt() != Some(expected_receipt) {
            return Ok(None);
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let Some(lease) = store.acquire_quality_read_lease()? else {
            return Ok(None);
        };
        if lease.generation() != expected_receipt.quality_generation
            || self.runtime_receipt.as_ref() != Some(expected_receipt)
        {
            return Ok(None);
        }
        Ok(Some(lease))
    }

    fn record_sustained_quality(
        &self,
        group: &str,
        target_identity: &str,
        result: &NodeSustainedQuality,
    ) -> Result<bool> {
        let Some(store) = &self.store else {
            #[cfg(test)]
            if self.allow_unpersisted_quality_for_test {
                return Ok(true);
            }
            return Ok(false);
        };
        store
            .record_sustained_quality(group, target_identity, result)
            .with_context(|| format!("failed to record sustained result for {}", result.name))
    }

    fn refresh_quick_history_cache_with_lease(
        &mut self,
        lease: &NodeQualityReadLease,
        group: &str,
        node: &str,
    ) {
        let Some(history) = self.store.as_ref().and_then(|store| {
            store
                .node_quick_history_with_lease(lease, group, node, 10)
                .ok()
        }) else {
            return;
        };
        self.quick_history_cache
            .insert((group.to_string(), node.to_string()), history);
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: String, client: AsyncClient) -> Self {
        let mut workflow = Self::new(
            base_url,
            client,
            None,
            sustained_target_identity(crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL)
                .expect("default sustained target is valid"),
        );
        workflow.allow_unpersisted_quality_for_test = true;
        workflow
    }

    #[cfg(test)]
    pub(crate) fn require_persisted_quality_for_test(&mut self) {
        self.allow_unpersisted_quality_for_test = false;
    }

    fn install_store(&mut self, store: Option<BenchmarkStore>) {
        self.runtime_receipt = None;
        for job in &self.jobs {
            if let Some(cancellation) = &job.cancellation {
                cancellation.store(true, Ordering::Relaxed);
            }
        }
        for job in &self.sustained_jobs {
            job.cancellation.store(true, Ordering::Relaxed);
        }
        for job in self.sustained_jobs.drain(..) {
            let _ = job.worker.join();
        }
        self.jobs.clear();
        self.summaries.clear();
        self.last_single_node = None;
        self.reachability_assessments = store
            .as_ref()
            .and_then(|store| store.latest_reachability_assessments().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(selector, assessment)| ((selector, assessment.name.clone()), assessment))
            .collect();
        self.sustained_quality = store
            .as_ref()
            .and_then(|store| {
                store
                    .latest_sustained_quality(&self.sustained_target_identity)
                    .ok()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|(selector, result)| ((selector, result.name.clone()), result))
            .collect();
        (self.quick_history_cache, self.sustained_stats_cache) = load_metric_caches(
            store.as_ref(),
            &self.reachability_assessments,
            &self.sustained_quality,
            &self.sustained_target_identity,
        );
        self.store = store;
    }

    fn quality_session_current(&self) -> bool {
        #[cfg(test)]
        if self.allow_unpersisted_quality_for_test {
            return true;
        }
        self.store
            .as_ref()
            .is_some_and(|store| store.quality_session_current().unwrap_or(false))
    }

    fn quality_generation_matches(&self, expected: u64) -> bool {
        #[cfg(test)]
        if self.allow_unpersisted_quality_for_test && self.store.is_none() {
            return expected == 0;
        }
        self.store
            .as_ref()
            .is_some_and(|store| store.quality_generation() == expected)
    }

    pub(crate) fn pause_quality_persistence(&mut self) {
        self.install_store(None);
    }

    #[cfg(test)]
    pub(crate) fn replace_store(&mut self, store: Option<BenchmarkStore>) {
        self.install_store(store);
    }

    #[cfg(test)]
    pub(crate) fn set_reachability_assessment(
        &mut self,
        group: &str,
        assessment: NodeReachabilityAssessment,
    ) {
        self.reachability_assessments
            .insert((group.to_string(), assessment.name.clone()), assessment);
    }

    #[cfg(test)]
    pub(crate) fn set_sustained_quality(&mut self, group: &str, result: NodeSustainedQuality) {
        self.sustained_quality
            .insert((group.to_string(), result.name.clone()), result);
    }

    #[cfg(test)]
    pub(crate) fn set_summary(&mut self, summary: BenchmarkSummary) {
        self.summaries.insert(summary.selector.clone(), summary);
    }

    #[cfg(test)]
    pub(crate) fn active_nodes(&self, group: &str) -> Option<&[String]> {
        self.jobs
            .iter()
            .find(|job| job.group == group)
            .map(|job| job.nodes.as_slice())
    }

    #[cfg(test)]
    pub(crate) fn active_sustained_nodes(&self, group: &str) -> Option<Vec<String>> {
        self.sustained_jobs
            .iter()
            .find(|job| job.group == group)
            .map(|job| job.outstanding_nodes.iter().cloned().collect())
    }

    #[cfg(test)]
    pub(crate) fn quality_persistence_enabled(&self) -> bool {
        self.store.is_some()
    }

    #[cfg(test)]
    pub(crate) fn adopt_runtime_receipt_for_test(
        &mut self,
        config_path: &Path,
        database_path: &Path,
        receipt: QualityRuntimeReceipt,
    ) -> Result<()> {
        self.skip_active_environment_check_for_test = true;
        self.adopt_runtime_receipt(config_path, database_path, receipt)
    }

    #[cfg(test)]
    pub(crate) fn persist_quality_projection_for_test(
        &self,
        group: &str,
        assessment: &NodeReachabilityAssessment,
        sustained: &NodeSustainedQuality,
    ) -> Result<()> {
        anyhow::ensure!(
            assessment.name == sustained.name,
            "test facts must name one node"
        );
        let receipt = self
            .runtime_receipt()
            .cloned()
            .context("test writer requires an adopted runtime receipt")?;
        anyhow::ensure!(
            self.record_reachability_assessment(Some(&receipt), group, assessment)?,
            "test reachability fact was fenced"
        );
        anyhow::ensure!(
            self.record_sustained_quality(group, &self.sustained_target_identity, sustained)?,
            "test sustained fact was fenced"
        );
        let delay_ms = assessment
            .attempts
            .iter()
            .find_map(|attempt| match attempt {
                ProbeOutcome::Reachable { delay_ms } => Some(*delay_ms),
                _ => None,
            });
        anyhow::ensure!(
            self.store
                .as_ref()
                .context("test writer requires node-quality persistence")?
                .record_benchmark(&BenchmarkRecord {
                    selector: group,
                    node: &assessment.name,
                    filter: "test",
                    delay_ms,
                    completed: true,
                    job_kind: "auto",
                })?,
            "test latency fact was fenced"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn background_projection_reload_count(&self) -> usize {
        self.background_projection_reload_count
    }

    #[cfg(test)]
    pub(crate) fn persist_benchmark_for_test(&self, node: &str) -> Result<Option<bool>> {
        self.store
            .as_ref()
            .map(|store| {
                store.record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node,
                    filter: "test",
                    delay_ms: Some(42),
                    completed: true,
                    job_kind: "test",
                })
            })
            .transpose()
    }

    #[cfg(test)]
    pub(crate) fn add_pending_job_for_test(&mut self, group: &str, node: &str) -> Arc<AtomicBool> {
        let (_sender, receiver) = mpsc::channel();
        let cancellation = Arc::new(AtomicBool::new(false));
        let (quality_receipt, quality_projection_current) = self.quality_job_scope();
        self.jobs.push(BenchmarkJob {
            group: group.to_string(),
            nodes: vec![node.to_string()],
            filter: "test".to_string(),
            kind: BenchmarkKind::AutoSelect,
            receiver,
            worker: std::thread::spawn(|| {}),
            cancellation: Some(cancellation.clone()),
            current_assessments: BTreeMap::new(),
            quality_receipt,
            quality_projection_current,
            auto_selection_round_id: Some(1),
        });
        cancellation
    }
}

impl Drop for BenchmarkWorkflow {
    fn drop(&mut self) {
        for job in &self.sustained_jobs {
            job.cancellation.store(true, Ordering::Relaxed);
        }
        for job in self.sustained_jobs.drain(..) {
            let _ = job.worker.join();
        }
    }
}

fn load_metric_caches(
    store: Option<&BenchmarkStore>,
    reachability: &BTreeMap<NodeMetricKey, NodeReachabilityAssessment>,
    sustained: &BTreeMap<NodeMetricKey, NodeSustainedQuality>,
    sustained_target_identity: &str,
) -> MetricCaches {
    let Some(store) = store else {
        return (BTreeMap::new(), BTreeMap::new());
    };
    let keys = reachability
        .keys()
        .chain(sustained.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut quick = BTreeMap::new();
    let mut sustained_stats = BTreeMap::new();
    for (group, node) in keys {
        if let Ok(history) = store.node_quick_history(&group, &node, 10) {
            quick.insert((group.clone(), node.clone()), history);
        }
        if let Ok(stats) =
            store.sustained_success_stats(&group, &node, sustained_target_identity, 10)
        {
            sustained_stats.insert((group, node), stats);
        }
    }
    (quick, sustained_stats)
}

fn compare_ratio_desc(
    left_successes: usize,
    left_attempts: usize,
    right_successes: usize,
    right_attempts: usize,
) -> CmpOrdering {
    let left_attempts = left_attempts.max(1);
    let right_attempts = right_attempts.max(1);
    right_successes
        .saturating_mul(left_attempts)
        .cmp(&left_successes.saturating_mul(right_attempts))
}

pub(crate) fn assessment_is_quick_eligible(assessment: &NodeReachabilityAssessment) -> bool {
    assessment.assessment.is_some_and(|assessment| {
        matches!(
            assessment,
            crate::controller::ReachabilityAssessment::StableReachable
                | crate::controller::ReachabilityAssessment::Reachable
        )
    })
}

fn assessment_warm_median_ms(assessment: &NodeReachabilityAssessment) -> Option<u64> {
    let mut warm = assessment
        .attempts
        .iter()
        .skip(1)
        .filter_map(|attempt| match attempt {
            ProbeOutcome::Reachable { delay_ms } => Some(*delay_ms),
            _ => None,
        })
        .collect::<Vec<_>>();
    if warm.is_empty() {
        warm.extend(
            assessment
                .attempts
                .iter()
                .filter_map(|attempt| match attempt {
                    ProbeOutcome::Reachable { delay_ms } => Some(*delay_ms),
                    _ => None,
                }),
        );
    }
    warm.sort_unstable();
    warm.get((warm.len().saturating_sub(1)) / 2).copied()
}

fn compare_optional_ascending(left: Option<u64>, right: Option<u64>) -> CmpOrdering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    }
}

fn normalize_controller_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::Sender;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::controller::ProbeOutcome;
    use crate::node_quality_path::{
        QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX, QUALITY_WRITE_BLOCK_SUFFIX,
    };
    use crate::sustained_quality::{SustainedCompletion, SustainedProbeOutcome};

    fn workflow(store: Option<BenchmarkStore>) -> BenchmarkWorkflow {
        let allow_unpersisted_quality_for_test = store.is_none();
        let mut workflow = BenchmarkWorkflow::new(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            store,
            sustained_target_identity(crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL)
                .unwrap(),
        );
        workflow.allow_unpersisted_quality_for_test = allow_unpersisted_quality_for_test;
        workflow
    }

    fn default_target_identity() -> String {
        sustained_target_identity(crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL).unwrap()
    }

    fn request(group: &str, nodes: &[&str]) -> BenchmarkRequest {
        BenchmarkRequest {
            selector: group.to_string(),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            request_timeout: 12.0,
            max_concurrency: 4,
            nodes: Some(nodes.iter().map(ToString::to_string).collect()),
        }
    }

    fn queue_job(
        workflow: &mut BenchmarkWorkflow,
        request: BenchmarkRequest,
        kind: BenchmarkKind,
        events: impl IntoIterator<Item = BenchmarkEvent>,
        keep_open: bool,
    ) -> Option<Sender<BenchmarkEvent>> {
        workflow.prepare_group(&request);
        let (sender, receiver) = mpsc::channel();
        for event in events {
            sender.send(event).expect("queue benchmark event");
        }
        let (quality_receipt, quality_projection_current) = workflow.quality_job_scope();
        let auto_selection_round_id = matches!(&kind, BenchmarkKind::AutoSelect).then(|| {
            let round_id = workflow.next_auto_selection_round_id;
            workflow.next_auto_selection_round_id =
                workflow.next_auto_selection_round_id.saturating_add(1);
            round_id
        });
        workflow.jobs.push(BenchmarkJob {
            group: request.selector.clone(),
            nodes: request.nodes.clone().unwrap_or_default(),
            filter: request.pattern.clone(),
            kind,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: None,
            current_assessments: BTreeMap::new(),
            quality_receipt,
            quality_projection_current,
            auto_selection_round_id,
        });
        keep_open.then_some(sender)
    }

    fn test_db_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "sing-box-tui-benchmark-workflow-{nanos}-{}.sqlite3",
            rand::random::<u64>()
        ))
    }

    fn install_test_runtime_receipt(workflow: &mut BenchmarkWorkflow, database_path: &Path) {
        let quality_generation = workflow
            .store
            .as_ref()
            .expect("test store")
            .quality_generation();
        workflow.runtime_receipt = Some(QualityRuntimeReceipt {
            canonical_config_path: database_path.with_extension("config.json"),
            canonical_database_path: database_path.to_path_buf(),
            controller_base_url: normalize_controller_base_url(&workflow.base_url),
            quality_generation,
            managed_pid: Some(std::process::id()),
        });
    }

    fn quality_marker_path(database_path: &Path) -> PathBuf {
        let mut path = database_path.as_os_str().to_os_string();
        path.push(QUALITY_WRITE_BLOCK_SUFFIX);
        PathBuf::from(path)
    }

    fn runtime_reload_fence_path(database_path: &Path) -> PathBuf {
        let mut path = database_path.as_os_str().to_os_string();
        path.push(QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX);
        PathBuf::from(path)
    }

    fn observed_runtime<T>(config_path: &Path, output: T) -> ManagedRuntimeObservation<T> {
        ManagedRuntimeObservation::new(
            output,
            config_path,
            "http://127.0.0.1:9992",
            Some(std::process::id()),
        )
    }

    fn remove_workflow_fixture(config_path: &Path, database_path: &Path) {
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_file(database_path);
        for suffix in [
            "-wal",
            "-shm",
            "-journal",
            QUALITY_WRITE_BLOCK_SUFFIX,
            QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX,
            ".node-quality-reconciliation.lock",
        ] {
            let mut path = database_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(path));
        }
        let mut config_lock = config_path.as_os_str().to_os_string();
        config_lock.push(".sing-box-tui-config-mutation.lock");
        let _ = std::fs::remove_file(PathBuf::from(config_lock));
    }

    #[test]
    fn production_open_binds_jsonc_config_but_waits_for_runtime_observation() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("config.json");
        std::fs::write(
            &config_path,
            r#"{
                // production configs may contain JSONC comments
                "outbounds": [
                    {"type":"selector","tag":"select","outbounds":["node-a","direct"]},
                    {"type":"direct","tag":"direct"},
                    {"type":"trojan","tag":"node-a","server":"same.example","server_port":443,"password":"secret"},
                ],
            }"#,
        )
        .expect("write JSONC active config");

        let (opened_tx, opened_rx) = std::sync::mpsc::sync_channel(1);
        let worker_config = config_path.clone();
        let worker_database = database_path.clone();
        let worker = thread::spawn(move || {
            let result = BenchmarkWorkflow::open(
                "http://127.0.0.1:9992".to_string(),
                AsyncClient::builder()
                    .no_proxy()
                    .build()
                    .expect("test client"),
                &worker_config,
                &worker_database,
                crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
            )
            .and_then(|mut workflow| {
                let initially_enabled = workflow.quality_persistence_enabled();
                let initially_fenced = runtime_reload_fence_path(&worker_database).exists();
                workflow.confirm_managed_runtime_reload(
                    &worker_config,
                    &worker_database,
                    || Ok(observed_runtime(&worker_config, ())),
                )?;
                Ok((
                    initially_enabled,
                    initially_fenced,
                    workflow.persist_benchmark_for_test("node-a")?,
                ))
            });
            opened_tx.send(result).expect("return startup result");
        });
        assert_eq!(
            opened_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("startup binding must not reacquire its own quality lock")
                .expect("open workflow and record bound fact"),
            (false, true, Some(true))
        );
        worker.join().expect("join startup binding worker");
        let store = BenchmarkStore::open(&database_path).expect("reopen bound store");
        assert_eq!(
            store
                .stored_node_identities()
                .expect("read startup identities")
                .into_iter()
                .map(|(tag, _)| tag)
                .collect::<Vec<_>>(),
            vec!["direct", "node-a", "select"]
        );

        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn production_open_without_active_config_keeps_persistence_disabled_and_marker_untouched() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("missing-config.json");
        let marker_path = quality_marker_path(&database_path);
        std::fs::write(&marker_path, b"preexisting marker").expect("seed quality marker");

        let workflow = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .expect("missing active config disables persistence");

        assert!(!workflow.quality_persistence_enabled());
        assert!(
            !database_path.exists(),
            "missing config must not create the DB"
        );
        assert_eq!(
            std::fs::read(&marker_path).expect("marker remains present"),
            b"preexisting marker"
        );
        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn production_open_with_invalid_active_config_does_not_initialize_or_unblock_quality() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("invalid-config.json");
        let marker_path = quality_marker_path(&database_path);
        std::fs::write(&config_path, b"{ invalid JSONC").expect("write invalid config");
        std::fs::write(&marker_path, b"preexisting marker").expect("seed quality marker");

        let error = match BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        ) {
            Ok(_) => panic!("invalid active config must reject quality persistence"),
            Err(error) => error,
        };

        assert!(format!("{error:#}").contains("failed to parse active sing-box config"));
        assert!(
            !database_path.exists(),
            "invalid config must not create the DB"
        );
        assert_eq!(
            std::fs::read(&marker_path).expect("marker remains present"),
            b"preexisting marker"
        );
        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn production_open_repairs_existing_marker_but_requires_runtime_observation() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("repair-config.json");
        std::fs::write(
            &config_path,
            r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#,
        )
        .expect("write active config");
        let store = BenchmarkStore::open(&database_path).expect("create unbound quality store");
        store
            .ensure_quality_writes_blocked()
            .expect("seed fail-closed marker");
        drop(store);

        let mut workflow = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .expect("committed config repairs quality binding");

        assert!(!workflow.quality_persistence_enabled());
        assert!(!quality_marker_path(&database_path).exists());
        assert!(runtime_reload_fence_path(&database_path).exists());
        workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || {
                Ok(observed_runtime(&config_path, ()))
            })
            .expect("observed runtime enables the repaired binding");
        assert!(workflow.quality_persistence_enabled());
        assert_eq!(
            workflow.persist_benchmark_for_test("direct").unwrap(),
            Some(true)
        );
        assert!(!runtime_reload_fence_path(&database_path).exists());
        drop(workflow);
        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn startup_identity_drift_stays_fenced_until_the_new_config_is_observed() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("startup-drift-config.json");
        let old_config = serde_json::json!({
            "outbounds": [{
                "type":"trojan", "tag":"node-a", "server":"old.example",
                "server_port":443, "password":"old-secret"
            }]
        });
        let new_config = serde_json::json!({
            "outbounds": [{
                "type":"trojan", "tag":"node-a", "server":"new.example",
                "server_port":443, "password":"new-secret"
            }]
        });
        std::fs::write(
            &config_path,
            serde_json::to_vec(&old_config).expect("serialize old config"),
        )
        .expect("write old active config");
        let stale_store = BenchmarkStore::open(&database_path).expect("open old runtime store");
        stale_store
            .reconcile_node_history(&old_config)
            .expect("bind the old runtime identities");

        std::fs::write(
            &config_path,
            serde_json::to_vec(&new_config).expect("serialize new config"),
        )
        .expect("replace active config outside the managed mutation path");
        let mut workflow = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .expect("startup binds the externally changed identity");

        assert!(!workflow.quality_persistence_enabled());
        assert!(runtime_reload_fence_path(&database_path).exists());
        assert!(
            !stale_store
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node: "node-a",
                    filter: "all",
                    delay_ms: Some(40),
                    completed: true,
                    job_kind: "auto",
                })
                .expect("old same-tag writer is safely rejected")
        );

        workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || {
                Ok(observed_runtime(&config_path, ()))
            })
            .expect("observing the new runtime releases the fence");
        assert!(workflow.quality_persistence_enabled());
        assert_eq!(
            workflow.persist_benchmark_for_test("node-a").unwrap(),
            Some(true)
        );

        drop(workflow);
        drop(stale_store);
        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn runtime_reload_fence_survives_restart_until_managed_config_is_observed() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("runtime-fence-config.json");
        std::fs::write(
            &config_path,
            r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#,
        )
        .expect("write active config");
        let store = BenchmarkStore::open(&database_path).expect("create quality store");
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"direct"}]
            }))
            .expect("bind active identities");
        store
            .ensure_runtime_reload_required()
            .expect("persist runtime reload fence");
        drop(store);

        // Opening a new TUI can bind the on-disk config, but it cannot prove that an external
        // controller has stopped serving the old same-tag outbound.
        let mut workflow = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .expect("restart binds config without clearing runtime fence");
        assert!(!workflow.quality_persistence_enabled());
        assert!(runtime_reload_fence_path(&database_path).exists());

        let error = workflow
            .confirm_managed_runtime_reload(
                &config_path,
                &database_path,
                || -> Result<ManagedRuntimeObservation<()>> {
                    anyhow::bail!("injected managed runtime readiness failure")
                },
            )
            .expect_err("failed runtime observation must retain the fence");
        assert!(format!("{error:#}").contains("readiness failure"));
        assert!(!workflow.quality_persistence_enabled());
        assert!(runtime_reload_fence_path(&database_path).exists());

        workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || {
                Ok(observed_runtime(&config_path, ()))
            })
            .expect("observed managed runtime clears the fence");
        assert!(workflow.quality_persistence_enabled());
        assert!(!runtime_reload_fence_path(&database_path).exists());
        assert_eq!(
            workflow.persist_benchmark_for_test("direct").unwrap(),
            Some(true)
        );

        drop(workflow);
        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn unchanged_generation_reload_failure_fences_other_process_writers() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("unchanged-reload-config.json");
        let config = serde_json::json!({
            "outbounds": [{"type":"direct", "tag":"direct"}]
        });
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let other_process_store = BenchmarkStore::open(&database_path).unwrap();
        other_process_store
            .reconcile_node_history(&config)
            .expect("bind unchanged identities");
        let generation = other_process_store.quality_generation();
        let mut workflow = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".into(),
            AsyncClient::builder().no_proxy().build().unwrap(),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .unwrap();
        assert!(workflow.quality_persistence_enabled());

        workflow
            .confirm_managed_runtime_reload(
                &config_path,
                &database_path,
                || -> Result<ManagedRuntimeObservation<()>> {
                    anyhow::bail!("injected unchanged-generation restart failure")
                },
            )
            .expect_err("failed restart must retain a cross-process fence");

        assert_eq!(other_process_store.quality_generation(), generation);
        assert!(runtime_reload_fence_path(&database_path).exists());
        assert!(
            !other_process_store
                .record_benchmark(&BenchmarkRecord {
                    selector: "select",
                    node: "direct",
                    filter: "all",
                    delay_ms: Some(20),
                    completed: true,
                    job_kind: "auto",
                })
                .expect("other process writer fails closed")
        );
        assert!(!workflow.quality_persistence_enabled());

        drop(workflow);
        drop(other_process_store);
        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn managed_observation_binds_config_controller_generation_and_pid() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("receipt-config.json");
        let other_config_path = database_path.with_extension("wrong-receipt-config.json");
        let config_text = r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#;
        std::fs::write(&config_path, config_text).unwrap();
        std::fs::write(&other_config_path, config_text).unwrap();
        let mut workflow = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".into(),
            AsyncClient::builder().no_proxy().build().unwrap(),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .unwrap();

        let wrong_controller = workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || {
                Ok(ManagedRuntimeObservation::new(
                    (),
                    &config_path,
                    "http://127.0.0.1:9993",
                    Some(std::process::id()),
                ))
            })
            .expect_err("wrong controller must not clear the runtime fence");
        assert!(format!("{wrong_controller:#}").contains("observed controller"));
        assert!(runtime_reload_fence_path(&database_path).exists());

        let wrong_config = workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || {
                Ok(ManagedRuntimeObservation::new(
                    (),
                    &other_config_path,
                    "http://127.0.0.1:9992",
                    Some(std::process::id()),
                ))
            })
            .expect_err("wrong config must not clear the runtime fence");
        assert!(format!("{wrong_config:#}").contains("observed config"));
        assert!(runtime_reload_fence_path(&database_path).exists());

        workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || {
                Ok(observed_runtime(&config_path, ()))
            })
            .expect("exact runtime observation clears the fence");
        let receipt = workflow.runtime_receipt().expect("receipt is installed");
        assert_eq!(
            receipt.canonical_config_path(),
            config_path.canonicalize().unwrap()
        );
        assert_eq!(receipt.controller_base_url, "http://127.0.0.1:9992");
        assert_eq!(receipt.managed_pid(), Some(std::process::id()));
        assert_eq!(
            receipt.quality_generation(),
            workflow.store.as_ref().unwrap().quality_generation()
        );

        drop(workflow);
        remove_workflow_fixture(&config_path, &database_path);
        let _ = std::fs::remove_file(other_config_path);
    }

    #[test]
    fn headless_adoption_rejects_missing_pid_stale_generation_and_reload_fence() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("headless-receipt-config.json");
        std::fs::write(
            &config_path,
            r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#,
        )
        .unwrap();
        let mut foreground = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".into(),
            AsyncClient::builder().no_proxy().build().unwrap(),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .unwrap();
        foreground
            .confirm_managed_runtime_reload(&config_path, &database_path, || {
                Ok(observed_runtime(&config_path, ()))
            })
            .unwrap();
        let receipt = foreground.runtime_receipt().unwrap().clone();

        let mut headless = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".into(),
            AsyncClient::builder().no_proxy().build().unwrap(),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .unwrap();
        headless.skip_active_environment_check_for_test = true;
        headless
            .adopt_runtime_receipt(&config_path, &database_path, receipt.clone())
            .expect("matching receipt is adopted without clearing a fence");
        assert_eq!(headless.runtime_receipt(), Some(&receipt));

        let mut missing_pid = receipt.clone();
        missing_pid.managed_pid = None;
        assert!(
            headless
                .adopt_runtime_receipt(&config_path, &database_path, missing_pid)
                .is_err()
        );
        assert!(!headless.quality_persistence_enabled());

        let mut stale_generation = receipt.clone();
        stale_generation.quality_generation += 1;
        headless.skip_active_environment_check_for_test = true;
        assert!(
            headless
                .adopt_runtime_receipt(&config_path, &database_path, stale_generation)
                .is_err()
        );
        assert!(!headless.quality_persistence_enabled());

        let store = BenchmarkStore::open(&database_path).unwrap();
        store.ensure_runtime_reload_required().unwrap();
        drop(store);
        headless.skip_active_environment_check_for_test = true;
        assert!(
            headless
                .adopt_runtime_receipt(&config_path, &database_path, receipt)
                .is_err()
        );
        assert!(runtime_reload_fence_path(&database_path).exists());
        assert!(!headless.quality_persistence_enabled());

        drop(headless);
        drop(foreground);
        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn poll_updates_summary_and_reports_typed_completion() {
        let mut workflow = workflow(None);
        queue_job(
            &mut workflow,
            request("select", &["node-a"]),
            BenchmarkKind::Group,
            [
                BenchmarkEvent::ReachabilityProgress(reachable("node-a", [42, 42, 42])),
                BenchmarkEvent::Finished,
            ],
            false,
        );

        let updates = workflow.poll();

        assert!(matches!(
            &updates[0],
            BenchmarkUpdate::Progress { group, best_label }
                if group == "select" && best_label == "3/3 stable reachable"
        ));
        assert!(matches!(
            &updates[1],
            BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
                group,
                assessed: 1,
                assessments,
                quality_current: true,
            }) if group == "select" && assessments.len() == 1
        ));
        assert!(workflow.active_nodes("select").is_none());
    }

    #[test]
    fn incomplete_current_run_never_uses_preserved_quick_evidence_for_follow_on_work() {
        let stale = reachable("node-a", [20, 30, 40]);
        let incomplete = NodeReachabilityAssessment {
            name: "node-a".into(),
            attempts: vec![ProbeOutcome::Timeout],
            assessment: None,
        };

        let mut group_workflow = workflow(None);
        group_workflow.set_reachability_assessment("select", stale.clone());
        queue_job(
            &mut group_workflow,
            request("select", &["node-a"]),
            BenchmarkKind::Group,
            [
                BenchmarkEvent::ReachabilityProgress(incomplete.clone()),
                BenchmarkEvent::Finished,
            ],
            false,
        );
        let group_updates = group_workflow.poll();
        assert!(!group_workflow.quick_eligible("select", "node-a"));
        assert!(matches!(
            group_updates.last(),
            Some(BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
                assessed: 1,
                assessments,
                ..
            })) if assessments.len() == 1 && assessments[0].assessment.is_none()
        ));

        let mut single_workflow = workflow(None);
        single_workflow.set_reachability_assessment("select", stale);
        queue_job(
            &mut single_workflow,
            request("select", &["node-a"]),
            BenchmarkKind::SingleNode {
                node: "node-a".into(),
            },
            [
                BenchmarkEvent::ReachabilityProgress(incomplete),
                BenchmarkEvent::Finished,
            ],
            false,
        );
        let single_updates = single_workflow.poll();
        assert!(!single_workflow.quick_eligible("select", "node-a"));
        assert!(matches!(
            single_updates.last(),
            Some(BenchmarkUpdate::Finished(BenchmarkCompletion::SingleNode {
                assessment: Some(assessment),
                ..
            })) if assessment.assessment.is_none()
        ));
    }

    #[test]
    fn fenced_quick_result_is_not_projected_or_completed_as_current() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"node-a"}]
            }))
            .unwrap();
        let mut workflow = workflow(Some(store));
        install_test_runtime_receipt(&mut workflow, &path);
        queue_job(
            &mut workflow,
            request("select", &["node-a"]),
            BenchmarkKind::Group,
            [
                BenchmarkEvent::ReachabilityProgress(reachable("node-a", [20, 30, 40])),
                BenchmarkEvent::Finished,
            ],
            false,
        );
        workflow
            .store
            .as_ref()
            .unwrap()
            .ensure_quality_writes_blocked()
            .unwrap();

        let updates = workflow.poll();

        assert!(matches!(
            updates.last(),
            Some(BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
                quality_current: false,
                assessments,
                ..
            })) if assessments.is_empty()
        ));
        assert!(workflow.reachability_assessments.is_empty());
        workflow
            .store
            .as_ref()
            .unwrap()
            .clear_quality_write_block()
            .unwrap();
        drop(workflow);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generation_change_after_quick_persist_blocks_memory_projection_and_completion() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"node-a"}]
            }))
            .unwrap();
        let mut workflow = workflow(Some(store));
        install_test_runtime_receipt(&mut workflow, &path);
        let sender = queue_job(
            &mut workflow,
            request("select", &["node-a"]),
            BenchmarkKind::Group,
            [BenchmarkEvent::ReachabilityProgress(reachable(
                "node-a",
                [20, 30, 40],
            ))],
            true,
        )
        .expect("keep quick job open");

        let progress = workflow.poll();
        assert!(matches!(
            progress.as_slice(),
            [BenchmarkUpdate::Progress { .. }]
        ));
        assert!(workflow.reachability_assessments.is_empty());
        workflow
            .store
            .as_ref()
            .unwrap()
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"node-b"}]
            }))
            .unwrap();
        sender.send(BenchmarkEvent::Finished).unwrap();

        let completion = workflow.poll();

        assert!(matches!(
            completion.last(),
            Some(BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
                quality_current: false,
                assessments,
                ..
            })) if assessments.is_empty()
        ));
        assert!(workflow.reachability_assessments.is_empty());
        drop(sender);
        drop(workflow);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn completed_quick_batch_projects_history_under_one_read_lease() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"node-a"}]
            }))
            .unwrap();
        let mut workflow = workflow(Some(store));
        install_test_runtime_receipt(&mut workflow, &path);
        queue_job(
            &mut workflow,
            request("select", &["node-a"]),
            BenchmarkKind::Group,
            [
                BenchmarkEvent::ReachabilityProgress(reachable("node-a", [20, 30, 40])),
                BenchmarkEvent::Finished,
            ],
            false,
        );

        let updates = workflow.poll();

        assert!(matches!(
            updates.last(),
            Some(BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
                quality_current: true,
                assessed: 1,
                assessments,
                ..
            })) if assessments.len() == 1
        ));
        assert!(workflow.quick_eligible("select", "node-a"));
        assert_eq!(workflow.quick_history("select", "node-a").rounds, 1);
        drop(workflow);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn progress_is_recorded_with_stable_run_metadata() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [
                    {"type":"selector", "tag":"select", "outbounds":["美国-a"]},
                    {"type":"direct", "tag":"美国-a"}
                ]
            }))
            .expect("bind test node identities");
        let workflow = workflow(Some(store));
        workflow.record_result(
            "select",
            "美国",
            &BenchmarkKind::AutoSelect,
            &BenchmarkResult {
                name: "美国-a".to_string(),
                delay: Some(88),
                completed: true,
            },
        );

        let store = BenchmarkStore::open(&path).expect("reopen benchmark store");
        let rows = store.recent_benchmarks(10).expect("read rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].selector, "select");
        assert_eq!(rows[0].node, "美国-a");
        assert_eq!(rows[0].filter, "美国");
        assert_eq!(rows[0].delay_ms, Some(88));
        assert!(rows[0].completed);
        assert_eq!(rows[0].job_kind, "auto");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn manual_run_blocks_background_snapshot_replacement() {
        let mut workflow = workflow(None);
        let sender = queue_job(
            &mut workflow,
            request("select", &["node-a"]),
            BenchmarkKind::SingleNode {
                node: "node-a".to_string(),
            },
            [],
            true,
        );
        let snapshot = BackgroundLatencySnapshot {
            quality_generation: 0,
            selector: "select".to_string(),
            current: Some("node-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![BackgroundLatencyResult {
                name: "node-a".to_string(),
                delay: Some(88),
                completed: true,
            }],
        };

        assert!(
            !workflow
                .apply_background_snapshot(&snapshot, "美国")
                .expect("manual foreground job rejects background snapshot")
        );
        let result = workflow
            .summary("select")
            .and_then(|summary| summary.find_result("node-a"))
            .expect("pending node");
        assert!(!result.completed);
        assert_eq!(result.delay, None);
        drop(sender);
    }

    #[test]
    fn repeated_single_node_run_is_debounced_before_spawning() {
        let mut workflow = workflow(None);
        workflow.last_single_node = Some((
            "select".to_string(),
            "node-a".to_string(),
            Instant::now() - Duration::from_millis(1),
        ));

        let outcome = workflow.start_single_node(
            BenchmarkRequest {
                max_concurrency: 1,
                ..request("select", &["node-a"])
            },
            "node-a".to_string(),
        );

        assert_eq!(outcome, BenchmarkStart::Debounced);
        assert!(workflow.active_nodes("select").is_none());
    }

    #[test]
    fn repeating_a_running_manual_assessment_requests_cancellation() {
        let mut workflow = workflow(None);
        let request = request("select", &["node-a"]);
        assert_eq!(
            workflow.start_group(request.clone()),
            BenchmarkStart::Started
        );
        assert_eq!(
            workflow.start_group(request),
            BenchmarkStart::CancellationRequested
        );
        assert!(
            workflow.jobs[0]
                .cancellation
                .as_ref()
                .is_some_and(|token| token.load(Ordering::Relaxed))
        );
    }

    fn reachable(node: &str, delays: [u64; 3]) -> NodeReachabilityAssessment {
        NodeReachabilityAssessment::from_attempts(
            node.to_string(),
            delays
                .into_iter()
                .map(|delay_ms| ProbeOutcome::Reachable { delay_ms })
                .collect(),
        )
    }

    fn sustained(node: &str, throughput: u64) -> NodeSustainedQuality {
        NodeSustainedQuality {
            name: node.to_string(),
            outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
                first_byte_ms: 100,
                completion_ms: 600,
                bytes_read: 512 * 1024,
                throughput_bytes_per_second: throughput,
            }),
        }
    }

    #[test]
    fn switching_sustained_target_replaces_projection_and_partitions_new_history() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"node-a"}]
            }))
            .unwrap();
        let quality_generation = store.quality_generation();
        let target_a = "https://a.example.test/payload?bytes=524288";
        let target_b = "https://b.example.test/payload?bytes=524288";
        let target_a_identity = sustained_target_identity(target_a).unwrap();
        let target_b_identity = sustained_target_identity(target_b).unwrap();
        let assessment = reachable("node-a", [20, 30, 40]);
        store
            .record_reachability_assessment("select", &assessment)
            .unwrap();
        store
            .record_sustained_quality("select", &target_a_identity, &sustained("node-a", 1_000))
            .unwrap();
        let mut workflow = BenchmarkWorkflow::new(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder().no_proxy().build().unwrap(),
            Some(store),
            target_a_identity,
        );

        assert_eq!(
            workflow.streaming_members("select", &["node-a".into()]),
            ["node-a"]
        );
        workflow.activate_sustained_target(target_b).unwrap();
        assert!(workflow.sustained_quality("select", "node-a").is_none());
        assert!(
            workflow
                .streaming_members("select", &["node-a".into()])
                .is_empty()
        );

        let (sender, receiver) = mpsc::channel();
        sender
            .send(SustainedProbeEvent::Progress(sustained("node-a", 2_000)))
            .unwrap();
        sender.send(SustainedProbeEvent::Finished).unwrap();
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["node-a".into()]),
            target_identity: target_b_identity.clone(),
            quality_generation,
            kind: SustainedKind::SingleNode,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::new(AtomicBool::new(false)),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });
        workflow.poll();

        assert_eq!(
            workflow.streaming_members("select", &["node-a".into()]),
            ["node-a"]
        );
        drop(workflow);
        let store = BenchmarkStore::open(&path).unwrap();
        assert_eq!(
            store
                .latest_sustained_quality(&target_b_identity)
                .unwrap()
                .len(),
            1
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fail_closed_marker_rejects_sustained_memory_projection_and_completion() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"node-a"}]
            }))
            .unwrap();
        let generation = store.quality_generation();
        let mut workflow = BenchmarkWorkflow::new(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder().no_proxy().build().unwrap(),
            Some(store),
            default_target_identity(),
        );
        workflow
            .store
            .as_ref()
            .unwrap()
            .ensure_quality_writes_blocked()
            .unwrap();
        let (sender, receiver) = mpsc::channel();
        sender
            .send(SustainedProbeEvent::Progress(sustained("node-a", 2_000)))
            .unwrap();
        sender.send(SustainedProbeEvent::Finished).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["node-a".into()]),
            target_identity: default_target_identity(),
            quality_generation: generation,
            kind: SustainedKind::SingleNode,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::clone(&cancellation),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });

        assert!(workflow.poll().is_empty());
        assert!(cancellation.load(Ordering::Relaxed));
        assert!(workflow.sustained_quality.is_empty());
        workflow
            .store
            .as_ref()
            .unwrap()
            .clear_quality_write_block()
            .unwrap();
        assert!(
            workflow
                .store
                .as_ref()
                .unwrap()
                .latest_sustained_quality(&default_target_identity())
                .unwrap()
                .is_empty()
        );
        drop(workflow);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reconciliation_after_sustained_commit_blocks_projection_and_completion() {
        let path = test_db_path();
        let initial_config = serde_json::json!({
            "outbounds": [{"type":"direct", "tag":"node-a"}]
        });
        let next_config = serde_json::json!({
            "outbounds": [{"type":"direct", "tag":"node-b"}]
        });
        let store = BenchmarkStore::open(&path).unwrap();
        store.reconcile_node_history(&initial_config).unwrap();
        let generation = store.quality_generation();
        let reconciler = BenchmarkStore::open(&path).unwrap();
        let mut workflow = BenchmarkWorkflow::new(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder().no_proxy().build().unwrap(),
            Some(store),
            default_target_identity(),
        );
        workflow.after_sustained_persist_hook = Some(Box::new(move || {
            reconciler
                .reconcile_node_history(&next_config)
                .expect("inject reconciliation after sustained commit");
        }));
        let (sender, receiver) = mpsc::channel();
        sender
            .send(SustainedProbeEvent::Progress(sustained("node-a", 2_000)))
            .unwrap();
        sender.send(SustainedProbeEvent::Finished).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["node-a".into()]),
            target_identity: default_target_identity(),
            quality_generation: generation,
            kind: SustainedKind::SingleNode,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::clone(&cancellation),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });

        let updates = workflow.poll();

        assert!(updates.is_empty());
        assert!(cancellation.load(Ordering::Relaxed));
        assert!(workflow.sustained_quality.is_empty());
        assert!(workflow.sustained_stats_cache.is_empty());
        assert!(workflow.sustained_jobs.is_empty());
        drop(workflow);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn start_sustained_rejects_nodes_outside_the_bound_generation_before_spawn() {
        let database_path = test_db_path();
        let config_path = database_path.with_extension("sustained-membership-config.json");
        std::fs::write(
            &config_path,
            r#"{"outbounds":[{"type":"direct","tag":"node-a"}]}"#,
        )
        .unwrap();
        let mut workflow = BenchmarkWorkflow::open(
            "http://127.0.0.1:9992".into(),
            AsyncClient::builder().no_proxy().build().unwrap(),
            &config_path,
            &database_path,
            crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL,
        )
        .unwrap();
        workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || {
                Ok(observed_runtime(&config_path, ()))
            })
            .unwrap();

        let started = workflow
            .start_sustained(
                SustainedProbeRequest {
                    selector: "select".into(),
                    nodes: vec!["not-in-generation".into()],
                    target_url: crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL.into(),
                    config_path: config_path.clone(),
                    sing_box_executable: std::env::current_exe().unwrap(),
                },
                SustainedKind::SingleNode,
            )
            .unwrap();

        assert_eq!(started, BenchmarkStart::NoCandidates);
        assert!(workflow.sustained_jobs.is_empty());
        drop(workflow);
        remove_workflow_fixture(&config_path, &database_path);
    }

    #[test]
    fn pausing_quality_cancels_quick_and_sustained_jobs_and_clears_every_projection() {
        let mut workflow = workflow(None);
        let quick_cancellation = workflow.add_pending_job_for_test("select", "node-a");
        workflow.set_reachability_assessment("select", reachable("node-a", [20, 30, 40]));
        workflow.set_sustained_quality("select", sustained("node-a", 2_000));
        workflow.quick_history_cache.insert(
            ("select".into(), "node-a".into()),
            NodeQuickHistory {
                successful_rounds: 1,
                rounds: 1,
                ..NodeQuickHistory::default()
            },
        );
        workflow.sustained_stats_cache.insert(
            ("select".into(), "node-a".into()),
            SustainedSuccessStats {
                successes: 1,
                attempts: 1,
            },
        );
        let (_sender, receiver) = mpsc::channel();
        let sustained_cancellation = Arc::new(AtomicBool::new(false));
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["node-a".into()]),
            target_identity: default_target_identity(),
            quality_generation: 0,
            kind: SustainedKind::Automatic,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::clone(&sustained_cancellation),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });

        workflow.pause_quality_persistence();

        assert!(quick_cancellation.load(Ordering::Relaxed));
        assert!(sustained_cancellation.load(Ordering::Relaxed));
        assert!(workflow.jobs.is_empty());
        assert!(workflow.sustained_jobs.is_empty());
        assert!(workflow.reachability_assessments.is_empty());
        assert!(workflow.sustained_quality.is_empty());
        assert!(workflow.quick_history_cache.is_empty());
        assert!(workflow.sustained_stats_cache.is_empty());
        assert!(workflow.last_single_node.is_none());
    }

    #[test]
    fn streaming_view_requires_both_gates_and_uses_lexicographic_ranking() {
        let mut workflow = workflow(None);
        for (node, p95, cold) in [
            ("throughput", 200, 80),
            ("success", 200, 80),
            ("p95", 100, 80),
            ("cold", 100, 40),
            ("degraded", 10, 10),
        ] {
            workflow.set_reachability_assessment("select", reachable(node, [cold, p95, p95]));
            workflow.set_sustained_quality(
                "select",
                sustained(node, if node == "throughput" { 2_000 } else { 1_000 }),
            );
        }
        workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment::from_attempts(
                "degraded".into(),
                vec![
                    ProbeOutcome::Reachable { delay_ms: 10 },
                    ProbeOutcome::Timeout,
                    ProbeOutcome::Timeout,
                ],
            ),
        );
        workflow.sustained_stats_cache.insert(
            ("select".into(), "success".into()),
            SustainedSuccessStats {
                successes: 2,
                attempts: 2,
            },
        );
        for node in ["p95", "cold"] {
            workflow.sustained_stats_cache.insert(
                ("select".into(), node.into()),
                SustainedSuccessStats {
                    successes: 1,
                    attempts: 2,
                },
            );
        }

        let members = ["cold", "degraded", "p95", "success", "throughput"].map(str::to_string);
        assert_eq!(
            workflow.streaming_members("select", &members),
            vec!["throughput", "success", "cold", "p95"]
        );
    }

    #[test]
    fn streaming_projection_remains_coherent_after_generation_change() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).unwrap();
        store
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"node-a"}]
            }))
            .unwrap();
        let assessment = reachable("node-a", [20, 30, 40]);
        store
            .record_reachability_assessment("select", &assessment)
            .unwrap();
        store
            .record_sustained_quality(
                "select",
                &default_target_identity(),
                &sustained("node-a", 2_000),
            )
            .unwrap();
        let mut workflow = workflow(Some(store));
        workflow.sustained_stats_cache.clear();

        let sustained_quality = workflow
            .sustained_quality
            .get(&("select".into(), "node-a".into()))
            .unwrap();
        assert_eq!(
            workflow.sustained_stats_current("select", "node-a", sustained_quality),
            SustainedSuccessStats {
                successes: 1,
                attempts: 1,
            }
        );

        let projection = workflow.streaming_projection("select", &["node-a".into()]);
        assert_eq!(projection.len(), 1);

        let reconciler = BenchmarkStore::open(&path).unwrap();
        reconciler
            .reconcile_node_history(&serde_json::json!({
                "outbounds": [{"type":"direct", "tag":"node-b"}]
            }))
            .unwrap();

        // The old frame owns one internally consistent copy, while a fresh frame observes the
        // generation fence and fails closed instead of mixing old membership with missing facts.
        assert_eq!(projection[0].name, "node-a");
        assert_eq!(projection[0].assessment, assessment);
        assert_eq!(
            projection[0].completion.throughput_bytes_per_second,
            512 * 1024 * 1_000 / 500
        );
        assert_eq!(
            projection[0].sustained_stats,
            SustainedSuccessStats {
                successes: 1,
                attempts: 1,
            }
        );
        assert!(
            workflow
                .streaming_projection("select", &["node-a".into()])
                .is_empty()
        );

        drop(reconciler);
        drop(workflow);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn automatic_sustained_scope_is_current_union_top_five_eligible() {
        let mut workflow = workflow(None);
        let members = (0..8)
            .map(|index| format!("node-{index}"))
            .collect::<Vec<_>>();
        for (index, node) in members.iter().enumerate().skip(1) {
            workflow.set_reachability_assessment(
                "select",
                reachable(node, [20 + index as u64, 30, 40]),
            );
        }
        workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment::from_attempts(
                "node-0".into(),
                vec![ProbeOutcome::Timeout; 3],
            ),
        );

        let current_assessments = members
            .iter()
            .filter_map(|node| workflow.reachability_assessment("select", node).cloned())
            .collect::<Vec<_>>();
        let selected = workflow.automatic_sustained_candidates(
            "select",
            Some("node-0"),
            &members,
            &current_assessments,
        );
        assert_eq!(selected.len(), 6);
        assert_eq!(selected[0], "node-0");
        assert_eq!(selected[1..], members[1..6]);
    }

    #[test]
    fn automatic_sustained_scope_keeps_current_without_a_complete_quick_assessment() {
        let workflow = workflow(None);
        let members = vec!["current".to_string(), "eligible".to_string()];
        let eligible = reachable("eligible", [20, 30, 40]);

        let selected = workflow.automatic_sustained_candidates(
            "select",
            Some("current"),
            &members,
            &[eligible],
        );

        assert_eq!(selected, vec!["current", "eligible"]);
    }

    #[test]
    fn sustained_completion_counts_only_results_from_the_current_job() {
        let mut workflow = workflow(None);
        workflow.set_sustained_quality("select", sustained("node-a", 1_000));
        let (sender, receiver) = mpsc::channel();
        sender
            .send(SustainedProbeEvent::Progress(NodeSustainedQuality {
                name: "node-a".into(),
                outcome: SustainedProbeOutcome::RuntimeFailed {
                    detail: "runtime unavailable".into(),
                },
            }))
            .unwrap();
        sender.send(SustainedProbeEvent::Finished).unwrap();
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["node-a".into()]),
            target_identity: default_target_identity(),
            quality_generation: 0,
            kind: SustainedKind::Automatic,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::new(AtomicBool::new(false)),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });

        let updates = workflow.poll();
        assert!(matches!(
            updates.last(),
            Some(BenchmarkUpdate::Finished(BenchmarkCompletion::Sustained {
                completed: 0,
                attempted: 0,
                infrastructure_failures: 1,
                cancelled: 0,
                ..
            }))
        ));
        assert!(workflow.sustained_quality("select", "node-a").is_some());
    }

    #[test]
    fn sustained_completion_separates_attributable_infrastructure_and_cancelled_results() {
        let mut workflow = workflow(None);
        let (sender, receiver) = mpsc::channel();
        for result in [
            sustained("completed", 1_000),
            NodeSustainedQuality {
                name: "transfer".into(),
                outcome: SustainedProbeOutcome::TransferFailed {
                    detail: "short body".into(),
                },
            },
            NodeSustainedQuality {
                name: "infra".into(),
                outcome: SustainedProbeOutcome::RuntimeFailed {
                    detail: "unavailable".into(),
                },
            },
            NodeSustainedQuality {
                name: "cancelled".into(),
                outcome: SustainedProbeOutcome::Cancelled,
            },
        ] {
            sender.send(SustainedProbeEvent::Progress(result)).unwrap();
        }
        sender.send(SustainedProbeEvent::Finished).unwrap();
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from([
                "completed".into(),
                "transfer".into(),
                "infra".into(),
                "cancelled".into(),
            ]),
            target_identity: default_target_identity(),
            quality_generation: 0,
            kind: SustainedKind::Automatic,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::new(AtomicBool::new(false)),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });

        let updates = workflow.poll();
        assert!(matches!(
            updates.last(),
            Some(BenchmarkUpdate::Finished(BenchmarkCompletion::Sustained {
                completed: 1,
                attempted: 2,
                infrastructure_failures: 1,
                cancelled: 1,
                ..
            }))
        ));
    }

    #[test]
    fn automatic_sustained_work_keeps_non_overlapping_nodes() {
        let mut workflow = workflow(None);
        let (keep_open, receiver) = mpsc::channel();
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["current".into()]),
            target_identity: default_target_identity(),
            quality_generation: 0,
            kind: SustainedKind::SingleNode,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::new(AtomicBool::new(false)),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });
        let start = workflow
            .start_sustained(
                SustainedProbeRequest {
                    selector: "select".into(),
                    nodes: vec!["current".into(), "node-b".into(), "node-c".into()],
                    target_url: crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL.into(),
                    config_path: PathBuf::from("/definitely-missing-sustained-config"),
                    sing_box_executable: std::env::current_exe().unwrap(),
                },
                SustainedKind::Automatic,
            )
            .unwrap();

        assert_eq!(start, BenchmarkStart::Started);
        assert_eq!(workflow.sustained_jobs.len(), 2);
        assert_eq!(
            workflow.sustained_jobs[1].outstanding_nodes,
            BTreeSet::from(["node-b".into(), "node-c".into()])
        );
        drop(keep_open);
    }

    #[test]
    fn single_sustained_reuses_overlapping_automatic_work_without_cancelling_its_batch() {
        let mut workflow = workflow(None);
        let cancellation = Arc::new(AtomicBool::new(false));
        let (keep_open, receiver) = mpsc::channel();
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["node-a".into(), "node-b".into()]),
            target_identity: default_target_identity(),
            quality_generation: 0,
            kind: SustainedKind::Automatic,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::clone(&cancellation),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });

        let start = workflow
            .start_sustained(
                SustainedProbeRequest {
                    selector: "select".into(),
                    nodes: vec!["node-a".into()],
                    target_url: crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL.into(),
                    config_path: PathBuf::from("/definitely-missing-sustained-config"),
                    sing_box_executable: std::env::current_exe().unwrap(),
                },
                SustainedKind::SingleNode,
            )
            .unwrap();

        assert_eq!(start, BenchmarkStart::AlreadyRunning);
        assert!(!cancellation.load(Ordering::Relaxed));
        assert_eq!(workflow.sustained_jobs.len(), 1);
        assert_eq!(
            workflow.sustained_jobs[0].outstanding_nodes,
            BTreeSet::from(["node-a".into(), "node-b".into()])
        );
        drop(keep_open);
    }

    #[test]
    fn single_sustained_retries_after_automatic_node_reported_but_batch_is_still_running() {
        let mut workflow = workflow(None);
        let cancellation = Arc::new(AtomicBool::new(false));
        let (keep_open, receiver) = mpsc::channel();
        keep_open
            .send(SustainedProbeEvent::Progress(NodeSustainedQuality {
                name: "node-a".into(),
                outcome: SustainedProbeOutcome::RuntimeFailed {
                    detail: "unavailable".into(),
                },
            }))
            .unwrap();
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["node-a".into(), "node-b".into()]),
            target_identity: default_target_identity(),
            quality_generation: 0,
            kind: SustainedKind::Automatic,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::clone(&cancellation),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });
        workflow.poll();
        assert_eq!(
            workflow.sustained_jobs[0].outstanding_nodes,
            BTreeSet::from(["node-b".into()])
        );

        let start = workflow
            .start_sustained(
                SustainedProbeRequest {
                    selector: "select".into(),
                    nodes: vec!["node-a".into()],
                    target_url: crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL.into(),
                    config_path: PathBuf::from("/definitely-missing-sustained-config"),
                    sing_box_executable: std::env::current_exe().unwrap(),
                },
                SustainedKind::SingleNode,
            )
            .unwrap();

        assert_eq!(start, BenchmarkStart::Started);
        assert!(!cancellation.load(Ordering::Relaxed));
        assert_eq!(workflow.sustained_jobs.len(), 2);
        assert!(
            workflow.sustained_jobs[0]
                .outstanding_nodes
                .contains("node-b")
        );
        drop(keep_open);
    }

    #[test]
    fn cancelled_job_from_target_switch_does_not_block_a_new_single_probe() {
        let mut workflow = workflow(None);
        let cancellation = Arc::new(AtomicBool::new(false));
        let (keep_open, receiver) = mpsc::channel();
        workflow.sustained_jobs.push(SustainedJob {
            group: "select".into(),
            outstanding_nodes: BTreeSet::from(["node-a".into()]),
            target_identity: default_target_identity(),
            quality_generation: 0,
            kind: SustainedKind::Automatic,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: Arc::clone(&cancellation),
            attributable_attempts: 0,
            completed_results: 0,
            infrastructure_failures: 0,
            cancelled_results: 0,
            persistence_failed: false,
        });

        workflow
            .activate_sustained_target("https://other.example.test/payload?bytes=524288")
            .unwrap();
        assert!(cancellation.load(Ordering::Relaxed));
        workflow
            .activate_sustained_target(crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL)
            .unwrap();
        let start = workflow
            .start_sustained(
                SustainedProbeRequest {
                    selector: "select".into(),
                    nodes: vec!["node-a".into()],
                    target_url: crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL.into(),
                    config_path: PathBuf::from("/definitely-missing-sustained-config"),
                    sing_box_executable: std::env::current_exe().unwrap(),
                },
                SustainedKind::SingleNode,
            )
            .unwrap();

        assert_eq!(start, BenchmarkStart::Started);
        assert_eq!(workflow.sustained_jobs.len(), 2);
        drop(keep_open);
    }
}
