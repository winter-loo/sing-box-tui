use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::Client as AsyncClient;

use crate::auto_pick::{BackgroundLatencyResult, BackgroundLatencySnapshot};
use crate::config::parse_sing_box_config_text;
use crate::config_mutation::lock_config_mutation_for;
use crate::controller::{
    BenchmarkEvent, BenchmarkRequest, BenchmarkResult, BenchmarkSummary,
    NodeReachabilityAssessment, spawn_benchmark_worker, spawn_reachability_assessment_worker,
};
use crate::defaults::SINGLE_NODE_RETEST_DEBOUNCE;
use crate::node_quality_path::ensure_active_config_paths_are_distinct;
use crate::storage::{
    BenchmarkRecord, BenchmarkStore, NodeLatencySample, lock_node_quality_reconciliation,
};

pub(crate) struct BenchmarkWorkflow {
    base_url: String,
    client: AsyncClient,
    summaries: BTreeMap<String, BenchmarkSummary>,
    reachability_assessments: BTreeMap<(String, String), NodeReachabilityAssessment>,
    jobs: Vec<BenchmarkJob>,
    last_single_node: Option<(String, String, Instant)>,
    store: Option<BenchmarkStore>,
    latency_order: bool,
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
    Progress { group: String, best_label: String },
    Finished(BenchmarkCompletion),
    Disconnected { group: String },
}

pub(crate) enum BenchmarkCompletion {
    Group {
        group: String,
        assessed: usize,
    },
    AutoSelect {
        group: String,
        summary: BenchmarkSummary,
    },
    SingleNode {
        group: String,
        node: String,
        assessment: Option<NodeReachabilityAssessment>,
    },
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
}

impl BenchmarkWorkflow {
    pub(crate) fn open(
        base_url: String,
        client: AsyncClient,
        config_path: &Path,
        database_path: &Path,
    ) -> Result<Self> {
        Self::open_with_binding_hook(base_url, client, config_path, database_path, || {})
    }

    fn open_with_binding_hook<Hook>(
        base_url: String,
        client: AsyncClient,
        config_path: &Path,
        database_path: &Path,
        after_config_read: Hook,
    ) -> Result<Self>
    where
        Hook: FnOnce(),
    {
        ensure_active_config_paths_are_distinct(config_path, database_path, &[])?;
        let _config_guard = lock_config_mutation_for(config_path)?;
        let config_text = match fs::read_to_string(config_path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Self::new(base_url, client, None));
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
        Reload: FnOnce() -> Result<T>,
    {
        ensure_active_config_paths_are_distinct(config_path, database_path, &[])?;
        let _config_guard = lock_config_mutation_for(config_path)?;
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

        // Do not clear this cross-process fence merely because a TUI restarted. The callback's
        // successful readiness observation, while the exact config remains locked, is the point
        // at which old same-tag controller results become attributable to the bound identities.
        let result = reload()?;
        store
            .clear_runtime_reload_required()
            .context("managed sing-box loaded the config but quality persistence stayed fenced")?;
        drop(quality_guard);
        self.install_store(Some(store));
        Ok(result)
    }

    #[cfg(test)]
    pub(crate) fn open_with_binding_hook_for_test<Hook>(
        base_url: String,
        client: AsyncClient,
        config_path: &Path,
        database_path: &Path,
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
            after_config_read,
        )
    }

    fn new(base_url: String, client: AsyncClient, store: Option<BenchmarkStore>) -> Self {
        let reachability_assessments = store
            .as_ref()
            .and_then(|store| store.latest_reachability_assessments().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(selector, assessment)| ((selector, assessment.name.clone()), assessment))
            .collect();
        Self {
            base_url,
            client,
            summaries: BTreeMap::new(),
            reachability_assessments,
            jobs: Vec::new(),
            last_single_node: None,
            store,
            latency_order: false,
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

    pub(crate) fn latency_order(&self) -> bool {
        self.latency_order
    }

    pub(crate) fn toggle_latency_order(&mut self) -> bool {
        self.latency_order = !self.latency_order;
        self.latency_order
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

    pub(crate) fn poll(&mut self) -> Vec<BenchmarkUpdate> {
        let mut updates = Vec::new();
        let mut finished_indexes = Vec::new();

        for index in 0..self.jobs.len() {
            let mut finished = false;
            loop {
                match self.jobs[index].receiver.try_recv() {
                    Ok(BenchmarkEvent::Progress(result)) => {
                        let group = self.jobs[index].group.clone();
                        let filter = self.jobs[index].filter.clone();
                        let kind = self.jobs[index].kind.clone();
                        let best_label = self
                            .summaries
                            .get_mut(&group)
                            .map(|summary| {
                                summary.update_result(result.clone());
                                summary.best_label()
                            })
                            .unwrap_or_else(|| "pending".to_string());
                        self.record_result(&group, &filter, &kind, &result);
                        updates.push(BenchmarkUpdate::Progress { group, best_label });
                    }
                    Ok(BenchmarkEvent::ReachabilityProgress(assessment)) => {
                        let group = self.jobs[index].group.clone();
                        let best_label = assessment.compact_evidence();
                        self.record_reachability_assessment(&group, &assessment);
                        if assessment.assessment.is_some() {
                            self.reachability_assessments
                                .insert((group.clone(), assessment.name.clone()), assessment);
                        }
                        updates.push(BenchmarkUpdate::Progress { group, best_label });
                    }
                    Ok(BenchmarkEvent::Finished) => {
                        finished = true;
                        let group = self.jobs[index].group.clone();
                        let kind = self.jobs[index].kind.clone();
                        if let Some(completion) = self.completion(group, kind) {
                            updates.push(BenchmarkUpdate::Finished(completion));
                        }
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
        updates
    }

    pub(crate) fn apply_background_snapshot(
        &mut self,
        latency: &BackgroundLatencySnapshot,
        active_filter: &str,
    ) -> bool {
        if latency.pattern != active_filter
            || self.jobs.iter().any(|job| {
                job.group == latency.selector && !matches!(job.kind, BenchmarkKind::AutoSelect)
            })
        {
            return false;
        }

        let summary = self
            .summaries
            .entry(latency.selector.clone())
            .or_insert_with(|| BenchmarkSummary::empty(latency.selector.clone()));
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
        true
    }

    pub(crate) fn background_snapshot(&self, group: &str) -> Option<BackgroundLatencySnapshot> {
        let summary = self.summaries.get(group)?;
        Some(BackgroundLatencySnapshot {
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
        let (tx, receiver) = mpsc::channel();
        let (worker, cancellation) = if matches!(kind, BenchmarkKind::AutoSelect) {
            (
                spawn_benchmark_worker(self.base_url.clone(), self.client.clone(), request, tx),
                None,
            )
        } else {
            let cancellation = Arc::new(AtomicBool::new(false));
            (
                spawn_reachability_assessment_worker(
                    self.base_url.clone(),
                    self.client.clone(),
                    request,
                    tx,
                    cancellation.clone(),
                ),
                Some(cancellation),
            )
        };
        self.jobs.push(BenchmarkJob {
            group,
            nodes,
            filter,
            kind,
            receiver,
            worker,
            cancellation,
        });
    }

    fn completion(&self, group: String, kind: BenchmarkKind) -> Option<BenchmarkCompletion> {
        Some(match kind {
            BenchmarkKind::Group => BenchmarkCompletion::Group {
                assessed: self
                    .jobs
                    .iter()
                    .find(|job| job.group == group)
                    .map(|job| {
                        job.nodes
                            .iter()
                            .filter(|node| self.reachability_assessment(&group, node).is_some())
                            .count()
                    })
                    .unwrap_or_default(),
                group,
            },
            BenchmarkKind::AutoSelect => BenchmarkCompletion::AutoSelect {
                summary: self.summaries.get(&group)?.clone(),
                group,
            },
            BenchmarkKind::SingleNode { node } => BenchmarkCompletion::SingleNode {
                assessment: self.reachability_assessment(&group, &node).cloned(),
                group,
                node,
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

    fn record_reachability_assessment(&self, group: &str, assessment: &NodeReachabilityAssessment) {
        let Some(store) = &self.store else { return };
        store
            .record_reachability_assessment(group, assessment)
            .map(|_| ())
            .with_context(|| {
                format!(
                    "failed to record reachability assessment for {}",
                    assessment.name
                )
            })
            .unwrap_or_else(|error| eprintln!("warning: {error:#}"));
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: String, client: AsyncClient) -> Self {
        Self::new(base_url, client, None)
    }

    fn install_store(&mut self, store: Option<BenchmarkStore>) {
        for job in &self.jobs {
            if let Some(cancellation) = &job.cancellation {
                cancellation.store(true, Ordering::Relaxed);
            }
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
        self.store = store;
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
    pub(crate) fn quality_persistence_enabled(&self) -> bool {
        self.store.is_some()
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
        self.jobs.push(BenchmarkJob {
            group: group.to_string(),
            nodes: vec![node.to_string()],
            filter: "test".to_string(),
            kind: BenchmarkKind::AutoSelect,
            receiver,
            worker: std::thread::spawn(|| {}),
            cancellation: Some(cancellation.clone()),
        });
        cancellation
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc::Sender;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::node_quality_path::{
        QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX, QUALITY_WRITE_BLOCK_SUFFIX,
    };

    fn workflow(store: Option<BenchmarkStore>) -> BenchmarkWorkflow {
        BenchmarkWorkflow::new(
            "http://127.0.0.1:9992".to_string(),
            AsyncClient::builder()
                .no_proxy()
                .build()
                .expect("test client"),
            store,
        )
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
        workflow.jobs.push(BenchmarkJob {
            group: request.selector.clone(),
            nodes: request.nodes.clone().unwrap_or_default(),
            filter: request.pattern.clone(),
            kind,
            receiver,
            worker: thread::spawn(|| {}),
            cancellation: None,
        });
        keep_open.then_some(sender)
    }

    fn test_db_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-benchmark-workflow-{nanos}.sqlite3"))
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
            )
            .and_then(|mut workflow| {
                let initially_enabled = workflow.quality_persistence_enabled();
                let initially_fenced = runtime_reload_fence_path(&worker_database).exists();
                workflow.confirm_managed_runtime_reload(
                    &worker_config,
                    &worker_database,
                    || Ok(()),
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
        )
        .expect("committed config repairs quality binding");

        assert!(!workflow.quality_persistence_enabled());
        assert!(!quality_marker_path(&database_path).exists());
        assert!(runtime_reload_fence_path(&database_path).exists());
        workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || Ok(()))
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
            .confirm_managed_runtime_reload(&config_path, &database_path, || Ok(()))
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
        )
        .expect("restart binds config without clearing runtime fence");
        assert!(!workflow.quality_persistence_enabled());
        assert!(runtime_reload_fence_path(&database_path).exists());

        let error = workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || -> Result<()> {
                anyhow::bail!("injected managed runtime readiness failure")
            })
            .expect_err("failed runtime observation must retain the fence");
        assert!(format!("{error:#}").contains("readiness failure"));
        assert!(!workflow.quality_persistence_enabled());
        assert!(runtime_reload_fence_path(&database_path).exists());

        workflow
            .confirm_managed_runtime_reload(&config_path, &database_path, || Ok(()))
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
    fn poll_updates_summary_and_reports_typed_completion() {
        let mut workflow = workflow(None);
        queue_job(
            &mut workflow,
            request("select", &["node-a"]),
            BenchmarkKind::Group,
            [
                BenchmarkEvent::Progress(BenchmarkResult {
                    name: "node-a".to_string(),
                    delay: Some(42),
                    completed: true,
                }),
                BenchmarkEvent::Finished,
            ],
            false,
        );

        let updates = workflow.poll();

        assert!(matches!(
            &updates[0],
            BenchmarkUpdate::Progress { group, best_label }
                if group == "select" && best_label == "node-a (42ms)"
        ));
        assert!(matches!(
            &updates[1],
            BenchmarkUpdate::Finished(BenchmarkCompletion::Group { group, assessed: 0 })
                if group == "select"
        ));
        assert!(workflow.active_nodes("select").is_none());
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
        let mut workflow = workflow(Some(store));
        queue_job(
            &mut workflow,
            request("select", &["美国-a"]),
            BenchmarkKind::AutoSelect,
            [BenchmarkEvent::Progress(BenchmarkResult {
                name: "美国-a".to_string(),
                delay: Some(88),
                completed: true,
            })],
            false,
        );

        workflow.poll();

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

        assert!(!workflow.apply_background_snapshot(&snapshot, "美国"));
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
}
