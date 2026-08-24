use std::collections::BTreeMap;
use std::sync::mpsc::{self, TryRecvError};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::Client as AsyncClient;

use crate::auto_pick::{BackgroundLatencyResult, BackgroundLatencySnapshot};
use crate::controller::{
    BenchmarkEvent, BenchmarkRequest, BenchmarkResult, BenchmarkSummary, spawn_benchmark_worker,
};
use crate::defaults::SINGLE_NODE_RETEST_DEBOUNCE;
use crate::storage::{
    BenchmarkRecord, BenchmarkStore, NodeLatencySample, default_benchmark_db_path,
};

pub(crate) struct BenchmarkWorkflow {
    base_url: String,
    client: AsyncClient,
    summaries: BTreeMap<String, BenchmarkSummary>,
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
}

pub(crate) enum BenchmarkUpdate {
    Progress { group: String, best_label: String },
    Finished(BenchmarkCompletion),
    Disconnected { group: String },
}

pub(crate) enum BenchmarkCompletion {
    Group {
        group: String,
        best: Option<BenchmarkResult>,
    },
    AutoSelect {
        group: String,
        summary: BenchmarkSummary,
    },
    SingleNode {
        group: String,
        node: String,
        result: Option<BenchmarkResult>,
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
}

impl BenchmarkWorkflow {
    pub(crate) fn open(base_url: String, client: AsyncClient) -> Result<Self> {
        Ok(Self::new(
            base_url,
            client,
            Some(BenchmarkStore::open(default_benchmark_db_path())?),
        ))
    }

    fn new(base_url: String, client: AsyncClient, store: Option<BenchmarkStore>) -> Self {
        Self {
            base_url,
            client,
            summaries: BTreeMap::new(),
            jobs: Vec::new(),
            last_single_node: None,
            store,
            latency_order: false,
        }
    }

    pub(crate) fn summary(&self, group: &str) -> Option<&BenchmarkSummary> {
        self.summaries.get(group)
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
        if self
            .jobs
            .iter()
            .any(|job| job.group == group && job.nodes.iter().any(|candidate| candidate == &node))
        {
            return BenchmarkStart::AlreadyRunning;
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
        if self.jobs.iter().any(|job| job.group == request.selector) {
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
        let worker =
            spawn_benchmark_worker(self.base_url.clone(), self.client.clone(), request, tx);
        self.jobs.push(BenchmarkJob {
            group,
            nodes,
            filter,
            kind,
            receiver,
            worker,
        });
    }

    fn completion(&self, group: String, kind: BenchmarkKind) -> Option<BenchmarkCompletion> {
        let summary = self.summaries.get(&group)?;
        Some(match kind {
            BenchmarkKind::Group => BenchmarkCompletion::Group {
                group,
                best: summary.best_success().cloned(),
            },
            BenchmarkKind::AutoSelect => BenchmarkCompletion::AutoSelect {
                group,
                summary: summary.clone(),
            },
            BenchmarkKind::SingleNode { node } => BenchmarkCompletion::SingleNode {
                group,
                result: summary.find_result(&node).cloned(),
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
            .with_context(|| format!("failed to record benchmark result for {}", result.name))
            .unwrap_or_else(|error| eprintln!("warning: {error:#}"));
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: String, client: AsyncClient) -> Self {
        Self::new(base_url, client, None)
    }

    #[cfg(test)]
    pub(crate) fn replace_store(&mut self, store: Option<BenchmarkStore>) {
        self.store = store;
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
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc::Sender;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;

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
            BenchmarkUpdate::Finished(BenchmarkCompletion::Group { group, best: Some(best) })
                if group == "select" && best.name == "node-a"
        ));
        assert!(workflow.active_nodes("select").is_none());
    }

    #[test]
    fn progress_is_recorded_with_stable_run_metadata() {
        let path = test_db_path();
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
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
}
