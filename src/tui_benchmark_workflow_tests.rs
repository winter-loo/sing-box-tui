use super::super::test_support::{internet_routes_app, test_app};
use crate::automatic_selection::{NodeViewId, RankingPolicy, SelectionScope};
use crate::benchmark_workflow::{BenchmarkCompletion, BenchmarkUpdate, SustainedKind};
use crate::controller::{
    ApiClient, BenchmarkRequest, ConnectionInfo, ConnectionMetadata, ConnectionsSnapshot,
    NodeReachabilityAssessment, ProbeOutcome, ProxyGroup,
};
use crate::storage::{StoredUsabilityProbeRun, UsabilityProbeFactRecord};
use crate::sustained_quality::{NodeSustainedQuality, SustainedCompletion, SustainedProbeOutcome};
use crate::usability_probe::{UsabilityProbeManifest, UsabilityProbeSource};
use crossterm::event::KeyCode;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct FakeController {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    address: std::net::SocketAddr,
    worker: Option<JoinHandle<()>>,
}

impl FakeController {
    fn start(final_current: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake controller");
        listener
            .set_nonblocking(true)
            .expect("set fake controller nonblocking");
        let address = listener.local_addr().expect("fake controller address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded_requests = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fake controller accept failed: {error}"),
                };
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                stream
                    .set_nonblocking(false)
                    .expect("make accepted fake connection blocking");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("set fake controller read timeout");
                let request = read_http_request(&mut stream);
                recorded_requests
                    .lock()
                    .expect("record fake request")
                    .push(request.clone());
                let request_line = request.lines().next().unwrap_or_default();
                let body = if request_line == "GET /configs HTTP/1.1" {
                    r#"{"mode":"rule","mode-list":["rule"]}"#.to_string()
                } else if request_line == "GET /proxies HTTP/1.1" {
                    format!(
                        r#"{{"proxies":{{"select":{{"name":"select","type":"Selector","now":"{final_current}","all":["node-a","node-b"]}}}}}}"#
                    )
                } else {
                    "{}".to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write fake controller response");
            }
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            stop,
            address,
            worker: Some(worker),
        }
    }
}

impl Drop for FakeController {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 2048];
    loop {
        let read = stream.read(&mut buffer).expect("read fake request");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("request content length")
                })
            })
            .unwrap_or(0);
        if request.len() >= header_end + content_length {
            break;
        }
    }
    String::from_utf8(request).expect("fake request is UTF-8")
}

fn reachability(name: &str, delays: [u64; 3]) -> NodeReachabilityAssessment {
    NodeReachabilityAssessment::from_attempts(
        name.to_string(),
        delays
            .into_iter()
            .map(|delay_ms| ProbeOutcome::Reachable { delay_ms })
            .collect(),
    )
}

fn auto_selection_scope() -> SelectionScope {
    SelectionScope {
        quality_generation: 0,
        selector: "select".to_string(),
        panel: NodeViewId::current_selector(),
        panel_revision: 0,
        current_node: "node-a".to_string(),
    }
}

fn seed_idle_transfer_window(app: &mut super::App) {
    let now = Instant::now();
    app.active_node_traffic.observe(
        auto_selection_scope(),
        now - Duration::from_secs(10),
        &ConnectionsSnapshot::default(),
    );
    app.active_node_traffic
        .observe(auto_selection_scope(), now, &ConnectionsSnapshot::default());
}

#[test]
fn single_node_benchmark_finish_does_not_flash() {
    let mut app = test_app();
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::SingleNode {
        group: "select".to_string(),
        node: "node-a".to_string(),
        assessment: Some(NodeReachabilityAssessment::from_attempts(
            "node-a".to_string(),
            vec![
                ProbeOutcome::Reachable { delay_ms: 42 },
                ProbeOutcome::Reachable { delay_ms: 45 },
                ProbeOutcome::Reachable { delay_ms: 48 },
            ],
        )),
        quality_current: true,
    }))
    .expect("apply completion");

    assert_eq!(
        app.status,
        "Reachability assessed select / node-a: 3/3 stable reachable"
    );
    assert!(app.flash.is_none());
}

#[test]
fn group_benchmark_finish_does_not_flash() {
    let mut app = test_app();
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
        group: "select".to_string(),
        assessed: 1,
        assessments: Vec::new(),
        quality_current: true,
    }))
    .expect("apply completion");

    assert_eq!(app.status, "Reachability assessed 1 node(s) in select");
    assert!(app.flash.is_none());
}

#[test]
fn stale_group_completion_is_nonfatal_and_does_not_start_sustained_work() {
    let mut app = test_app();
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
        group: "select".to_string(),
        assessed: 1,
        assessments: vec![NodeReachabilityAssessment::from_attempts(
            "node-a".to_string(),
            vec![
                ProbeOutcome::Reachable { delay_ms: 42 },
                ProbeOutcome::Reachable { delay_ms: 45 },
                ProbeOutcome::Reachable { delay_ms: 48 },
            ],
        )],
        quality_current: false,
    }))
    .expect("stale completion is handled as status, not an application error");

    assert_eq!(
        app.status,
        "Reachability results for select were discarded after the managed runtime changed; rerun T"
    );
    assert!(app.flash.is_none());
}

#[test]
fn runtime_change_after_current_completion_defers_sustained_without_exiting_tui() {
    let assessment = NodeReachabilityAssessment::from_attempts(
        "node-a".to_string(),
        vec![
            ProbeOutcome::Reachable { delay_ms: 42 },
            ProbeOutcome::Reachable { delay_ms: 45 },
            ProbeOutcome::Reachable { delay_ms: 48 },
        ],
    );
    let mut app = test_app();
    app.sustained_runtime_environment =
        Some((PathBuf::from("config.json"), PathBuf::from("sing-box")));
    app.benchmark_workflow.require_persisted_quality_for_test();

    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
        group: "select".to_string(),
        assessed: 1,
        assessments: vec![assessment.clone()],
        quality_current: true,
    }))
    .expect("group runtime race is nonfatal");
    assert!(app.status.contains("sustained probing deferred"));

    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::SingleNode {
        group: "select".to_string(),
        node: "node-a".to_string(),
        assessment: Some(assessment),
        quality_current: true,
    }))
    .expect("single-node runtime race is nonfatal");
    assert!(app.status.contains("sustained transfer deferred"));
    assert!(app.flash.is_none());
}

#[test]
fn auto_select_requires_two_public_completion_rounds_before_controller_write() {
    let controller = FakeController::start("node-b");
    let mut app = test_app();
    app.client = ApiClient::new(controller.base_url.clone(), None).expect("test API client");
    app.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
    app.benchmark_filter.clear();
    let current = reachability("node-a", [100, 100, 100]);
    let candidate = reachability("node-b", [70, 70, 70]);
    app.benchmark_workflow
        .set_reachability_assessment("select", current.clone());
    app.benchmark_workflow
        .set_reachability_assessment("select", candidate.clone());
    seed_idle_transfer_window(&mut app);

    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".to_string(),
        round_id: 41,
        assessments: vec![current.clone(), candidate.clone()],
        quality_current: true,
    }))
    .expect("first auto-selection round is handled");

    assert!(app.status.contains("awaiting confirmation 1/2"));
    assert!(
        controller
            .requests
            .lock()
            .expect("inspect fake requests")
            .is_empty(),
        "the first completed assessment must never write to the controller"
    );

    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".to_string(),
        round_id: 42,
        assessments: vec![current, candidate],
        quality_current: true,
    }))
    .expect("second auto-selection round switches");

    let requests = controller
        .requests
        .lock()
        .expect("inspect fake requests")
        .clone();
    let switch_requests = requests
        .iter()
        .filter(|request| request.starts_with("PUT /proxies/select "))
        .collect::<Vec<_>>();
    assert_eq!(switch_requests.len(), 1);
    assert!(switch_requests[0].contains(r#"{"name":"node-b"}"#));
    assert!(
        app.status
            .contains("Automatic selection switched select to node-b")
    );
    assert_eq!(app.groups[0].current.as_deref(), Some("node-b"));
    let explanation = app
        .last_auto_selection_explanation
        .as_ref()
        .expect("switch explanation is retained");
    assert!(explanation.matches("select", &NodeViewId::current_selector()));
    assert!(
        explanation
            .detail
            .contains("node-b won two complete rounds")
    );
}

#[test]
fn custom_auto_select_uses_included_members_and_resets_confirmation_for_a_new_run() {
    let controller = FakeController::start("node-b");
    let mut app = test_app();
    app.client = ApiClient::new(controller.base_url.clone(), None).expect("test API client");
    app.groups[0].members = vec![
        "node-a".into(),
        "node-b".into(),
        "node-c".into(),
        "node-d".into(),
    ];
    app.benchmark_filter.clear();
    let panel_id = NodeViewId::new("github-web").unwrap();
    app.usability_probe_manifests.push(UsabilityProbeManifest {
        id: panel_id.clone(),
        label: "GitHub Web".into(),
        ranking_policy: RankingPolicy::LowLatency,
        source: UsabilityProbeSource::Url("https://github.com/".into()),
        background: false,
        interval: None,
        result_ttl: None,
        timeout: std::time::Duration::from_secs(600),
        source_path: PathBuf::from("github-web.json"),
    });
    let install_run = |app: &mut super::App, run_id| {
        app.usability_probe_projection_cache.insert(
            (panel_id.clone(), "select".into()),
            StoredUsabilityProbeRun {
                run_id,
                completed_at_ms: run_id as u64,
                expires_at_ms: None,
                summary: None,
                latest_attempt: None,
                results: vec![
                    UsabilityProbeFactRecord {
                        node: "node-a".into(),
                        usable: false,
                        detail: Some("current is outside this panel".into()),
                    },
                    UsabilityProbeFactRecord {
                        node: "node-b".into(),
                        usable: true,
                        detail: Some("accepted".into()),
                    },
                    UsabilityProbeFactRecord {
                        node: "node-c".into(),
                        usable: false,
                        detail: Some("rejected".into()),
                    },
                ],
            },
        );
    };
    install_run(&mut app, 7);
    app.node_view_panel = super::super::NodeViewPanel::Custom(panel_id.clone());
    app.auto_select_enabled = true;
    app.auto_select_selector = Some("select".into());
    app.auto_select_node_view = panel_id.clone();
    app.auto_select_ranking_policy = RankingPolicy::LowLatency;

    let current = reachability("node-a", [100, 100, 100]);
    let included = reachability("node-b", [70, 70, 70]);
    let rejected = reachability("node-c", [10, 10, 10]);
    let untested = reachability("node-d", [5, 5, 5]);
    for assessment in [&current, &included, &rejected, &untested] {
        app.benchmark_workflow
            .set_reachability_assessment("select", assessment.clone());
    }
    app.maybe_start_auto_select_benchmark()
        .expect("custom evidence assessment starts");
    assert_eq!(
        app.benchmark_workflow.active_nodes("select"),
        Some(["node-a".to_string(), "node-b".to_string()].as_slice()),
        "only Included members plus the out-of-panel current node may be assessed"
    );

    let seed_idle = |app: &mut super::App, panel_revision| {
        let scope = SelectionScope {
            quality_generation: 0,
            selector: "select".into(),
            panel: panel_id.clone(),
            panel_revision,
            current_node: "node-a".into(),
        };
        let now = Instant::now();
        app.active_node_traffic.observe(
            scope.clone(),
            now - Duration::from_secs(10),
            &ConnectionsSnapshot::default(),
        );
        app.active_node_traffic
            .observe(scope, now, &ConnectionsSnapshot::default());
    };
    let assessments = vec![
        current.clone(),
        included.clone(),
        rejected.clone(),
        untested.clone(),
    ];
    seed_idle(&mut app, 7);
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".into(),
        round_id: 100,
        assessments: assessments.clone(),
        quality_current: true,
    }))
    .expect("first custom round is retained");
    assert!(app.status.contains("confirmation 1/2"));

    install_run(&mut app, 8);
    seed_idle(&mut app, 8);
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".into(),
        round_id: 101,
        assessments: assessments.clone(),
        quality_current: true,
    }))
    .expect("new manifest run resets confirmation");
    assert!(app.status.contains("confirmation 1/2"));
    assert!(
        controller
            .requests
            .lock()
            .expect("inspect pre-confirmation requests")
            .iter()
            .all(|request| !request.starts_with("PUT /proxies/select "))
    );

    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".into(),
        round_id: 102,
        assessments,
        quality_current: true,
    }))
    .expect("second round on the same manifest run switches");
    let switches = controller
        .requests
        .lock()
        .expect("inspect custom switch requests")
        .iter()
        .filter(|request| request.starts_with("PUT /proxies/select "))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(switches.len(), 1);
    assert!(switches[0].contains(r#"{"name":"node-b"}"#));
    assert!(!switches[0].contains("node-c"));
    assert!(!switches[0].contains("node-d"));
}

#[test]
fn streaming_auto_select_discovers_untested_nodes_before_they_become_selectable() {
    let controller = FakeController::start("node-b");
    let mut app = test_app();
    app.client = ApiClient::new(controller.base_url.clone(), None).expect("test API client");
    app.groups[0].members = vec![
        "node-a".to_string(),
        "node-b".to_string(),
        "node-c".to_string(),
    ];
    app.benchmark_filter = "node-b".to_string();
    app.move_node_view_next();
    app.auto_select_enabled = true;
    app.sustained_runtime_environment = Some((
        PathBuf::from("missing-test-config.json"),
        PathBuf::from("/usr/bin/false"),
    ));
    let current = reachability("node-a", [100, 100, 100]);
    let candidate = reachability("node-b", [70, 70, 70]);
    let panel_rejected = reachability("node-c", [10, 10, 10]);
    for assessment in [&current, &candidate, &panel_rejected] {
        app.benchmark_workflow
            .set_reachability_assessment("select", assessment.clone());
    }

    app.maybe_start_auto_select_benchmark()
        .expect("streaming evidence assessment starts");
    assert_eq!(
        app.benchmark_workflow.active_nodes("select"),
        Some(["node-a".to_string(), "node-b".to_string()].as_slice()),
        "untested filter-matching nodes need quick evidence, while rejected nodes stay excluded"
    );

    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".to_string(),
        round_id: 70,
        assessments: vec![current.clone(), candidate.clone(), panel_rejected.clone()],
        quality_current: true,
    }))
    .expect("fresh streaming panel schedules missing sustained evidence");
    assert_eq!(
        app.benchmark_workflow.active_sustained_nodes("select"),
        Some(vec!["node-a".to_string(), "node-b".to_string()]),
        "bounded sustained discovery must include the untested panel candidate"
    );
    assert!(
        controller
            .requests
            .lock()
            .expect("inspect pre-evidence requests")
            .iter()
            .all(|request| !request.starts_with("PUT /proxies/select ")),
        "a discovery candidate is not selectable until its panel facts are complete"
    );

    for (node, throughput) in [("node-b", 1024 * 1024), ("node-c", 8 * 1024 * 1024)] {
        app.benchmark_workflow.set_sustained_quality(
            "select",
            NodeSustainedQuality {
                name: node.to_string(),
                outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
                    first_byte_ms: 100,
                    completion_ms: 600,
                    bytes_read: 512 * 1024,
                    throughput_bytes_per_second: throughput,
                }),
            },
        );
    }
    let scope = SelectionScope {
        quality_generation: 0,
        selector: "select".to_string(),
        panel: NodeViewId::streaming(),
        panel_revision: 0,
        current_node: "node-a".to_string(),
    };
    let now = Instant::now();
    app.active_node_traffic.observe(
        scope.clone(),
        now - Duration::from_secs(10),
        &ConnectionsSnapshot::default(),
    );
    app.active_node_traffic
        .observe(scope, now, &ConnectionsSnapshot::default());

    for round_id in [71, 72] {
        app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
            group: "select".to_string(),
            round_id,
            assessments: vec![current.clone(), candidate.clone(), panel_rejected.clone()],
            quality_current: true,
        }))
        .expect("completed streaming evidence is evaluated");
    }

    let switch_requests = controller
        .requests
        .lock()
        .expect("inspect streaming switch requests")
        .iter()
        .filter(|request| request.starts_with("PUT /proxies/select "))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(switch_requests.len(), 1);
    assert!(switch_requests[0].contains(r#"{"name":"node-b"}"#));
    assert!(!switch_requests[0].contains("node-c"));
}

#[test]
fn auto_select_public_completion_defers_while_current_node_has_active_traffic() {
    let mut app = test_app();
    app.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
    app.benchmark_filter.clear();
    let current = reachability("node-a", [100, 100, 100]);
    let candidate = reachability("node-b", [70, 70, 70]);
    app.benchmark_workflow
        .set_reachability_assessment("select", current.clone());
    app.benchmark_workflow
        .set_reachability_assessment("select", candidate.clone());
    let now = Instant::now();
    let baseline = ConnectionsSnapshot {
        connections: vec![ConnectionInfo {
            id: "current-flow".to_string(),
            download: 1_000,
            upload: 0,
            start: None,
            chains: vec!["node-a".to_string()],
            rule: None,
            rule_payload: None,
            metadata: ConnectionMetadata::default(),
        }],
        ..ConnectionsSnapshot::default()
    };
    let active = ConnectionsSnapshot {
        connections: vec![ConnectionInfo {
            id: "current-flow".to_string(),
            download: 1_000 + 64 * 1024 + 1,
            ..baseline.connections[0].clone()
        }],
        ..ConnectionsSnapshot::default()
    };
    app.active_node_traffic.observe(
        auto_selection_scope(),
        now - Duration::from_secs(10),
        &baseline,
    );
    app.active_node_traffic
        .observe(auto_selection_scope(), now, &active);

    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".to_string(),
        round_id: 51,
        assessments: vec![current, candidate],
        quality_current: true,
    }))
    .expect("traffic deferral is a normal application result");

    assert!(
        app.status
            .contains("current-node connections grew by 65537 bytes in 10s"),
        "unexpected status: {}",
        app.status
    );
    assert_eq!(
        app.last_auto_selection_explanation
            .as_ref()
            .map(|explanation| explanation.detail.as_str()),
        Some("switch deferred: current-node connections grew by 65537 bytes in 10s")
    );
}

#[test]
fn emergency_requires_zero_growth_and_restarts_after_one_byte_of_traffic() {
    let controller = FakeController::start("node-b");
    let mut app = test_app();
    app.client = ApiClient::new(controller.base_url.clone(), None).expect("test API client");
    app.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
    app.benchmark_filter.clear();
    let current = NodeReachabilityAssessment::from_attempts(
        "node-a".to_string(),
        vec![ProbeOutcome::Timeout; 3],
    );
    let candidate = reachability("node-b", [70, 70, 70]);
    app.benchmark_workflow
        .set_reachability_assessment("select", current.clone());
    app.benchmark_workflow
        .set_reachability_assessment("select", candidate.clone());
    let now = Instant::now();
    let baseline = ConnectionsSnapshot {
        connections: vec![ConnectionInfo {
            id: "current-flow".to_string(),
            download: 1_000,
            upload: 0,
            start: None,
            chains: vec!["node-a".to_string()],
            rule: None,
            rule_payload: None,
            metadata: ConnectionMetadata::default(),
        }],
        ..ConnectionsSnapshot::default()
    };
    let one_byte = ConnectionsSnapshot {
        connections: vec![ConnectionInfo {
            download: 1_001,
            ..baseline.connections[0].clone()
        }],
        ..ConnectionsSnapshot::default()
    };
    app.active_node_traffic.observe(
        auto_selection_scope(),
        now - Duration::from_secs(10),
        &baseline,
    );
    app.active_node_traffic
        .observe(auto_selection_scope(), now, &one_byte);

    for round_id in [81, 82] {
        app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
            group: "select".to_string(),
            round_id,
            assessments: vec![current.clone(), candidate.clone()],
            quality_current: true,
        }))
        .expect("positive-growth emergency evidence is handled");
    }
    assert!(app.status.contains("measured 0/3"));
    assert!(app.status.contains("grew by 1 bytes"));
    assert!(
        controller
            .requests
            .lock()
            .expect("inspect anomaly requests")
            .iter()
            .all(|request| !request.starts_with("PUT /proxies/select ")),
        "even one observed byte must prevent an emergency controller write"
    );

    app.active_node_traffic = Default::default();
    let zero_now = Instant::now();
    app.active_node_traffic.observe(
        auto_selection_scope(),
        zero_now - Duration::from_secs(10),
        &ConnectionsSnapshot::default(),
    );
    app.active_node_traffic.observe(
        auto_selection_scope(),
        zero_now,
        &ConnectionsSnapshot::default(),
    );
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".to_string(),
        round_id: 83,
        assessments: vec![current.clone(), candidate.clone()],
        quality_current: true,
    }))
    .expect("first zero-growth outage round is retained");
    assert!(app.status.contains("emergency confirmation 1/2"));
    assert!(
        controller
            .requests
            .lock()
            .expect("inspect first recovery round")
            .iter()
            .all(|request| !request.starts_with("PUT /proxies/select "))
    );
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".to_string(),
        round_id: 84,
        assessments: vec![current, candidate],
        quality_current: true,
    }))
    .expect("second zero-growth outage round switches");
    let switch_requests = controller
        .requests
        .lock()
        .expect("inspect emergency switch")
        .iter()
        .filter(|request| request.starts_with("PUT /proxies/select "))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(switch_requests.len(), 1);
    assert!(switch_requests[0].contains(r#"{"name":"node-b"}"#));
}

#[test]
fn auto_select_reconciliation_race_is_explained_and_nonfatal() {
    let mut app = test_app();
    app.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
    app.benchmark_filter.clear();
    app.benchmark_workflow.require_persisted_quality_for_test();

    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::AutoSelect {
        group: "select".to_string(),
        round_id: 61,
        assessments: vec![
            reachability("node-a", [100, 100, 100]),
            reachability("node-b", [70, 70, 70]),
        ],
        quality_current: true,
    }))
    .expect("a lease race must not tear down the application loop");

    assert!(
        app.status
            .contains("Automatic selection deferred for select")
    );
    assert!(app.status.contains("confirmed managed runtime receipt"));
    assert!(
        app.last_auto_selection_explanation
            .as_ref()
            .is_some_and(|explanation| explanation
                .detail
                .contains("confirmed managed runtime receipt"))
    );
    assert!(app.flash.is_none());
}

#[test]
fn sustained_infrastructure_outcomes_are_reported_as_incomplete() {
    let mut app = test_app();
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::Sustained {
        group: "select".into(),
        kind: SustainedKind::Automatic,
        completed: 1,
        attempted: 2,
        infrastructure_failures: 1,
        cancelled: 1,
    }))
    .unwrap();

    assert!(app.status.contains("incomplete"));
    assert!(app.status.contains("1 infrastructure, 1 cancelled"));
}

#[test]
fn auto_select_uses_single_internet_route_selector_members() {
    let mut app = internet_routes_app();
    app.groups = vec![
        ProxyGroup {
            name: "手动选择".to_string(),
            kind: "Selector".to_string(),
            current: Some("airtcp".to_string()),
            members: vec!["airtcp".to_string()],
        },
        ProxyGroup {
            name: "airtcp".to_string(),
            kind: "Selector".to_string(),
            current: Some("香港-a".to_string()),
            members: vec!["香港-a".to_string(), "美国-b".to_string()],
        },
    ];
    app.internet_route_index = 0;
    app.auto_select_enabled = true;
    app.benchmark_filter = "美国".to_string();
    app.last_auto_select_benchmark = None;

    app.maybe_start_auto_select_benchmark()
        .expect("auto select starts");

    assert_eq!(
        app.benchmark_workflow.active_nodes("airtcp"),
        Some(["香港-a".to_string(), "美国-b".to_string()].as_slice())
    );
    assert!(
        app.benchmark_workflow
            .quick_probe_pending("airtcp", "香港-a")
    );
    assert!(
        app.benchmark_workflow
            .quick_probe_pending("airtcp", "美国-b")
    );
}

#[test]
fn auto_select_toggle_allows_empty_filter() {
    let mut app = test_app();
    app.benchmark_filter.clear();

    app.handle_key(KeyCode::Char('a')).expect("toggle handled");

    assert!(app.auto_select_enabled);
    assert_eq!(
        app.status,
        "Automatic selection enabled for select [current-selector / balanced] (all nodes, 20% material gate, two-round confirmation, every 30s)"
    );
}

#[test]
fn benchmark_request_carries_max_concurrency() {
    let request = BenchmarkRequest {
        selector: "select".to_string(),
        pattern: "美国".to_string(),
        url: "https://www.gstatic.com/generate_204".to_string(),
        timeout_ms: 5000,
        request_timeout: 12.0,
        max_concurrency: 3,
        nodes: None,
    };

    assert_eq!(request.max_concurrency, 3);
}

#[test]
fn group_quick_scope_always_includes_current_even_when_filter_excludes_it() {
    let mut app = test_app();
    app.groups[0].current = Some("current-hk".into());
    app.groups[0].members = vec!["current-hk".into(), "美国-b".into()];
    app.benchmark_filter = "美国".into();

    app.start_group_benchmark().unwrap();

    assert_eq!(
        app.benchmark_workflow.active_nodes("select"),
        Some(["current-hk".to_string(), "美国-b".to_string()].as_slice())
    );
}

#[test]
fn group_quick_scope_always_includes_current_outside_streaming_projection() {
    let mut app = test_app();
    app.groups[0].current = Some("current".into());
    app.groups[0].members = vec!["current".into(), "streaming".into(), "untested".into()];
    app.benchmark_filter.clear();
    app.benchmark_workflow.set_reachability_assessment(
        "select",
        NodeReachabilityAssessment::from_attempts(
            "streaming".into(),
            vec![
                ProbeOutcome::Reachable { delay_ms: 40 },
                ProbeOutcome::Reachable { delay_ms: 45 },
                ProbeOutcome::Reachable { delay_ms: 50 },
            ],
        ),
    );
    app.benchmark_workflow.set_sustained_quality(
        "select",
        NodeSustainedQuality {
            name: "streaming".into(),
            outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
                first_byte_ms: 100,
                completion_ms: 600,
                bytes_read: 512 * 1024,
                throughput_bytes_per_second: 1024 * 1024,
            }),
        },
    );
    app.move_node_view_next();

    app.start_group_benchmark().unwrap();

    assert_eq!(
        app.benchmark_workflow.active_nodes("select"),
        Some(
            [
                "current".to_string(),
                "streaming".to_string(),
                "untested".to_string(),
            ]
            .as_slice()
        )
    );
}
