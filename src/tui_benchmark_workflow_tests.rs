use super::super::test_support::{internet_routes_app, test_app};
use crate::auto_pick::AutoPickDecision;
use crate::benchmark_workflow::{BenchmarkCompletion, BenchmarkUpdate, SustainedKind};
use crate::controller::{
    BenchmarkRequest, BenchmarkResult, BenchmarkSummary, NodeReachabilityAssessment, ProbeOutcome,
    ProxyGroup,
};
use crate::sustained_quality::{NodeSustainedQuality, SustainedCompletion, SustainedProbeOutcome};
use crossterm::event::KeyCode;
use std::path::PathBuf;

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
fn toggling_latency_sort_mode_does_not_flash() {
    let mut app = test_app();
    app.set_status_with_flash("existing flash");

    app.toggle_latency_sort_mode();

    assert!(app.benchmark_workflow.latency_order());
    assert_eq!(
        app.status,
        "Sort order: LATENCY ORDER (sort successful nodes by delay, retain all members)"
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
fn auto_select_plan_selects_internet_route_even_when_node_is_kept() {
    let app = internet_routes_app();
    let group = app.group_by_name("AirTCP").expect("Internet Route").clone();
    let summary = BenchmarkSummary {
        selector: "AirTCP".to_string(),
        current: Some("air-1".to_string()),
        pattern: String::new(),
        url: "https://www.gstatic.com/generate_204".to_string(),
        timeout_ms: 5000,
        max_concurrency: 4,
        results: vec![
            BenchmarkResult {
                name: "air-1".to_string(),
                delay: Some(80),
                completed: true,
            },
            BenchmarkResult {
                name: "air-2".to_string(),
                delay: Some(90),
                completed: true,
            },
        ],
    };

    assert_eq!(
        app.auto_select_switch_plan(&group, &summary),
        AutoPickDecision {
            target_node: None,
            parent_switch: Some(("手动选择".to_string(), "AirTCP".to_string())),
        }
    );
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
        Some(["美国-b".to_string()].as_slice())
    );
    let summary = app
        .benchmark_workflow
        .summary("airtcp")
        .expect("airtcp summary");
    assert_eq!(summary.selector, "airtcp");
    assert_eq!(
        summary
            .results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>(),
        vec!["美国-b"]
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
        "Auto-pick enabled for select (all nodes, 600ms threshold, every 30s)"
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
