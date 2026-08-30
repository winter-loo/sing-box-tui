use super::super::test_support::{test_app, test_db_path};
use crate::automatic_selection::{AutoSelectionExplanation, NodeViewId, RankingPolicy};
use crate::benchmark_workflow::{BenchmarkWorkflow, ManagedRuntimeObservation};
use crate::controller::{NodeReachabilityAssessment, ProbeOutcome};
use crate::storage::{BenchmarkStore, UsabilityProbeRunFinalization};
use crate::sustained_quality::{
    DEFAULT_SUSTAINED_TARGET_URL, NodeSustainedQuality, SustainedCompletion, SustainedProbeOutcome,
    sustained_target_identity,
};
use crate::usability_probe::{UsabilityProbeManifest, UsabilityProbeSource};
use crossterm::event::KeyCode;
use reqwest::Client as AsyncClient;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[test]
fn scheduled_probe_status_advances_generation_without_auto_pick() {
    let mut last = "configuration applied".to_string();
    let mut generation = 4;

    super::publish_background_status_change(
        "Agy probe complete: 2/2 reported nodes usable",
        &mut last,
        &mut generation,
    );

    assert_eq!(generation, 5);
    assert_eq!(last, "Agy probe complete: 2/2 reported nodes usable");
}

#[test]
fn persisted_probe_start_restores_remaining_interval_after_worker_restart() {
    let now = Instant::now();
    let restored = super::super::usability_probe_workflow::restored_probe_start(
        now,
        1_000_000,
        970_000,
        Duration::from_secs(60),
    )
    .expect("recent persisted attempt has a monotonic anchor");

    assert_eq!(
        now.duration_since(restored),
        std::time::Duration::from_secs(30)
    );
    assert!(
        now.duration_since(restored) < std::time::Duration::from_secs(60),
        "a restarted worker must not immediately repeat a paid probe"
    );
    assert!(
        super::super::usability_probe_workflow::restored_probe_start(
            now,
            1_000_000,
            900_000,
            Duration::from_secs(60),
        )
        .is_none(),
        "an attempt older than the interval is immediately due"
    );
}

#[test]
fn restarted_worker_defers_recent_incomplete_probe_from_shared_sqlite() {
    let database_path = test_db_path();
    let config_path = database_path.with_extension("scheduled-probe-config.json");
    std::fs::write(
        &config_path,
        r#"{
            "outbounds": [
                {"type":"selector","tag":"select","outbounds":["node-a","node-b"]},
                {"type":"direct","tag":"node-a"},
                {"type":"direct","tag":"node-b"}
            ]
        }"#,
    )
    .expect("write scheduled-probe config");
    let mut workflow = open_persisted_workflow(&config_path, &database_path);
    workflow
        .confirm_managed_runtime_reload(&config_path, &database_path, || {
            Ok(ManagedRuntimeObservation::new(
                (),
                &config_path,
                "http://127.0.0.1:9992",
                Some(std::process::id()),
            ))
        })
        .expect("confirm scheduled-probe runtime");
    let (run_id, generation, process_lease) = workflow
        .begin_usability_probe_run("agy", "select")
        .expect("start prior scheduled attempt");
    workflow
        .finish_usability_probe_run_with_ttl(UsabilityProbeRunFinalization {
            run_id,
            generation,
            process_lease: &process_lease,
            complete: false,
            summary: None,
            diagnostic: Some("provider was temporarily unavailable"),
            facts: &[],
            result_ttl: Some(Duration::from_secs(300)),
        })
        .expect("persist prior incomplete attempt");

    let manifest_id = NodeViewId::new("agy").expect("manifest id");
    let mut app = test_app();
    app.benchmark_workflow = workflow;
    app.usability_probe_manifests.push(UsabilityProbeManifest {
        id: manifest_id.clone(),
        label: "Agy".to_string(),
        ranking_policy: RankingPolicy::Balanced,
        source: UsabilityProbeSource::Executable {
            executable: PathBuf::from("must-not-run"),
            args: Vec::new(),
        },
        background: true,
        interval: Some(Duration::from_secs(60)),
        result_ttl: Some(Duration::from_secs(300)),
        timeout: Duration::from_secs(60),
        source_path: PathBuf::from("agy.json"),
    });
    app.background_probe_enabled.insert(manifest_id.clone());
    app.background_probe_selectors
        .insert(manifest_id.clone(), "select".to_string());

    app.maybe_start_scheduled_usability_probe(Instant::now());

    assert!(app.usability_probe_job.is_none());
    assert!(
        app.last_background_probe_started
            .contains_key(&(manifest_id, "select".to_string())),
        "the restarted worker must restore the recent failed attempt into its monotonic schedule"
    );

    drop(app);
    remove_shared_quality_fixture(&config_path, &database_path);
}

fn open_persisted_workflow(config_path: &Path, database_path: &Path) -> BenchmarkWorkflow {
    BenchmarkWorkflow::open(
        "http://127.0.0.1:9992".to_string(),
        AsyncClient::builder()
            .no_proxy()
            .build()
            .expect("test HTTP client"),
        config_path,
        database_path,
        DEFAULT_SUSTAINED_TARGET_URL,
    )
    .expect("open persisted benchmark workflow")
}

fn completed_sustained(node: &str, throughput: u64) -> NodeSustainedQuality {
    let bytes_read = 512 * 1024;
    let transfer_ms = (bytes_read * 1_000 / throughput).max(1);
    NodeSustainedQuality {
        name: node.to_string(),
        outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
            first_byte_ms: 100,
            completion_ms: 100 + transfer_ms,
            bytes_read,
            throughput_bytes_per_second: throughput,
        }),
    }
}

fn remove_shared_quality_fixture(config_path: &Path, database_path: &Path) {
    let _ = std::fs::remove_file(config_path);
    let _ = std::fs::remove_file(database_path);
    for suffix in [
        "-wal",
        "-shm",
        "-journal",
        ".node-quality-writes-blocked",
        ".node-quality-runtime-reload-required",
        ".node-quality-reconciliation.lock",
    ] {
        let mut path = database_path.as_os_str().to_os_string();
        path.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(path));
    }
    let mut config_lock = config_path.as_os_str().to_os_string();
    config_lock.push(".sing-box-tui-config-mutation.lock");
    let _ = std::fs::remove_file(std::path::PathBuf::from(config_lock));
}

#[test]
fn background_explanation_round_trips_only_for_the_configured_selector_and_panel() {
    let mut app = test_app();
    app.auto_select_enabled = true;
    app.auto_select_selector = Some("select".to_string());
    let explanation = AutoSelectionExplanation {
        selector: "select".to_string(),
        panel: NodeViewId::current_selector(),
        detail: "node-b leads; awaiting confirmation 1/2".to_string(),
    };

    app.apply_background_auto_selection_explanation(Some(explanation.clone()));
    let snapshot = app.background_status_snapshot("testing".to_string(), 9);

    assert_eq!(snapshot.auto_selection_explanation, Some(explanation));
    assert!(
        serde_json::to_value(&snapshot)
            .expect("encode status snapshot")
            .get("latency")
            .is_none(),
        "worker status must not recreate the removed single-delay fact channel"
    );

    app.apply_background_auto_selection_explanation(Some(AutoSelectionExplanation {
        selector: "select".to_string(),
        panel: NodeViewId::streaming(),
        detail: "must not leak to the current-selector panel".to_string(),
    }));
    assert!(app.last_auto_selection_explanation.is_none());
}

#[test]
fn live_background_config_moves_selector_and_streaming_facts_to_one_target_partition() {
    let database_path = test_db_path();
    let store = BenchmarkStore::open(&database_path).expect("open target-partition store");
    store
        .reconcile_node_history(&serde_json::json!({
            "outbounds": [
                {"type":"selector", "tag":"select", "outbounds":["node-a","node-b","node-c"]},
                {"type":"direct", "tag":"node-a"},
                {"type":"direct", "tag":"node-b"},
                {"type":"direct", "tag":"node-c"}
            ]
        }))
        .expect("bind test nodes");
    let target_a = "https://a.example.test/payload?bytes=524288";
    let target_b = "https://b.example.test/payload?bytes=524288";
    assert!(
        store
            .record_sustained_quality(
                "select",
                &sustained_target_identity(target_a).expect("target A identity"),
                &completed_sustained("node-b", 1024 * 1024),
            )
            .expect("persist target A quality")
    );
    assert!(
        store
            .record_sustained_quality(
                "select",
                &sustained_target_identity(target_b).expect("target B identity"),
                &completed_sustained("node-c", 2 * 1024 * 1024),
            )
            .expect("persist target B quality")
    );

    let mut app = test_app();
    app.groups[0].members = vec![
        "node-a".to_string(),
        "node-b".to_string(),
        "node-c".to_string(),
    ];
    app.benchmark_filter = "node".to_string();
    app.benchmark_workflow.replace_store(Some(store));
    app.benchmark_workflow
        .activate_sustained_target(target_a)
        .expect("activate target A");
    app.sustained_target_url = target_a.to_string();
    for node in ["node-a", "node-b", "node-c"] {
        app.benchmark_workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment::from_attempts(
                node.to_string(),
                vec![
                    ProbeOutcome::Reachable { delay_ms: 40 },
                    ProbeOutcome::Reachable { delay_ms: 45 },
                    ProbeOutcome::Reachable { delay_ms: 50 },
                ],
            ),
        );
    }
    app.move_node_view_next();
    assert_eq!(app.displayed_members(), ["node-b"]);
    app.last_auto_select_benchmark = Some(Instant::now());
    app.last_auto_selection_explanation = Some(AutoSelectionExplanation {
        selector: "select".to_string(),
        panel: NodeViewId::streaming(),
        detail: "old target evidence".to_string(),
    });

    let mut target_b_config = app.auto_pick_config();
    target_b_config.enabled = true;
    target_b_config.selector = Some("select".to_string());
    target_b_config.active_node_view = NodeViewId::streaming();
    target_b_config.sustained_target_url = target_b.to_string();
    app.apply_background_auto_pick_config(target_b_config)
        .expect("live worker applies target B atomically");

    assert_eq!(app.sustained_target_url, target_b);
    assert_eq!(
        app.benchmark_workflow.sustained_target_identity(),
        sustained_target_identity(target_b)
            .expect("target B identity")
            .as_str()
    );
    assert!(
        app.benchmark_workflow
            .sustained_quality("select", "node-b")
            .is_none(),
        "target A projection must not remain selectable"
    );
    assert!(
        app.benchmark_workflow
            .sustained_quality("select", "node-c")
            .is_some(),
        "target B projection must drive the next Streaming decision"
    );
    assert_eq!(app.displayed_members(), ["node-c"]);
    assert!(app.last_auto_select_benchmark.is_none());
    assert!(app.last_auto_selection_explanation.is_none());
    assert_eq!(app.auto_pick_config().sustained_target_url, target_b);
    let status = app.background_status_snapshot("configuration applied".to_string(), 1);
    assert_eq!(status.sustained_target_url, target_b);
    assert_eq!(
        status.sustained_target_identity,
        sustained_target_identity(target_b).expect("target B identity")
    );

    let before_invalid = app.auto_pick_runtime_signature();
    let mut invalid = app.auto_pick_config();
    invalid.enabled = false;
    invalid.filter = "must-not-apply".to_string();
    invalid.sustained_target_url = "http://invalid.example.test/payload".to_string();
    let error = app
        .apply_background_auto_pick_config(invalid)
        .expect_err("invalid target is rejected before any config mutation");
    assert!(format!("{error:#}").contains("must use HTTPS"));
    assert_eq!(app.auto_pick_runtime_signature(), before_invalid);
    assert_eq!(app.displayed_members(), ["node-c"]);

    drop(app);
    remove_shared_quality_fixture(
        &database_path.with_extension("unused-config.json"),
        &database_path,
    );
}

#[test]
fn foreground_reloads_background_quality_from_shared_sqlite_for_streaming_and_detail() {
    let database_path = test_db_path();
    let config_path = database_path.with_extension("shared-background-config.json");
    std::fs::write(
        &config_path,
        r#"{
            "outbounds": [
                {"type":"selector","tag":"select","outbounds":["node-a","node-b"]},
                {"type":"direct","tag":"node-a"},
                {"type":"direct","tag":"node-b"}
            ]
        }"#,
    )
    .expect("write shared node-quality config");

    let mut foreground_workflow = open_persisted_workflow(&config_path, &database_path);
    foreground_workflow
        .confirm_managed_runtime_reload(&config_path, &database_path, || {
            Ok(ManagedRuntimeObservation::new(
                (),
                &config_path,
                "http://127.0.0.1:9992",
                Some(std::process::id()),
            ))
        })
        .expect("confirm foreground runtime");
    let receipt = foreground_workflow
        .runtime_receipt()
        .expect("foreground runtime receipt")
        .clone();
    let mut background_workflow = open_persisted_workflow(&config_path, &database_path);
    background_workflow
        .adopt_runtime_receipt_for_test(&config_path, &database_path, receipt)
        .expect("adopt foreground runtime in background workflow");

    let assessment = NodeReachabilityAssessment::from_attempts(
        "node-b".to_string(),
        vec![
            ProbeOutcome::Reachable { delay_ms: 70 },
            ProbeOutcome::Reachable { delay_ms: 75 },
            ProbeOutcome::Reachable { delay_ms: 80 },
        ],
    );
    let first_sustained = completed_sustained("node-b", 1024 * 1024);
    background_workflow
        .persist_quality_projection_for_test("select", &assessment, &first_sustained)
        .expect("background workflow persists quick and sustained facts");
    let mut foreground = test_app();
    foreground.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
    foreground.benchmark_filter = "node".to_string();
    foreground.benchmark_workflow = foreground_workflow;
    foreground.move_node_view_next();
    assert!(foreground.displayed_members().is_empty());

    let mut background = test_app();
    background.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
    background.benchmark_filter = "node".to_string();
    background.benchmark_workflow = background_workflow;
    let quality_generation = background
        .benchmark_workflow
        .runtime_receipt()
        .expect("background publishes a generation-scoped notification")
        .quality_generation();

    assert!(
        foreground
            .apply_background_quality_notification(quality_generation)
            .expect("foreground accepts same-generation quality notification")
    );
    assert_eq!(foreground.displayed_members(), vec!["node-b"]);
    assert_eq!(
        foreground
            .benchmark_workflow
            .background_projection_reload_count(),
        1
    );
    foreground
        .handle_key(KeyCode::Char('i'))
        .expect("open public quality detail");
    let detail = foreground
        .node_quality_detail
        .as_ref()
        .expect("quality detail opens");
    assert_eq!(detail.node, "node-b");
    assert_eq!(detail.reachability_assessment.as_ref(), Some(&assessment));
    assert_eq!(detail.sustained_quality.as_ref(), Some(&first_sustained));

    foreground
        .apply_background_quality_notification(quality_generation)
        .expect("unchanged poll remains cheap");
    assert_eq!(
        foreground
            .benchmark_workflow
            .background_projection_reload_count(),
        1,
        "unchanged SQLite data_version must not trigger another full projection reload"
    );

    let second_sustained = completed_sustained("node-b", 2 * 1024 * 1024);
    background
        .benchmark_workflow
        .persist_quality_projection_for_test("select", &assessment, &second_sustained)
        .expect("background commits a second fact revision");
    foreground
        .apply_background_quality_notification(quality_generation)
        .expect("next external commit is reloaded");
    assert_eq!(
        foreground
            .benchmark_workflow
            .sustained_quality("select", "node-b"),
        Some(&second_sustained)
    );
    assert_eq!(
        foreground
            .benchmark_workflow
            .background_projection_reload_count(),
        2
    );

    let third_sustained = completed_sustained("node-b", 4 * 1024 * 1024);
    background
        .benchmark_workflow
        .persist_quality_projection_for_test("select", &assessment, &third_sustained)
        .expect("background commits after the last accepted revision");
    assert!(
        foreground
            .apply_background_quality_notification(quality_generation + 1)
            .is_err(),
        "generation mismatch must fail closed"
    );
    assert_eq!(
        foreground
            .benchmark_workflow
            .sustained_quality("select", "node-b"),
        Some(&second_sustained),
        "failed refresh retains the prior coherent projection"
    );
    foreground
        .apply_background_quality_notification(quality_generation)
        .expect("failed generation check does not consume the pending data version");
    assert_eq!(
        foreground
            .benchmark_workflow
            .sustained_quality("select", "node-b"),
        Some(&third_sustained)
    );
    assert_eq!(
        foreground
            .benchmark_workflow
            .background_projection_reload_count(),
        3
    );

    drop(background);
    drop(foreground);
    remove_shared_quality_fixture(&config_path, &database_path);
}
