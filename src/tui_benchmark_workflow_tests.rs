use super::super::test_support::{internet_routes_app, test_app};
use crate::auto_pick::AutoPickDecision;
use crate::benchmark_workflow::{BenchmarkCompletion, BenchmarkUpdate};
use crate::controller::{BenchmarkRequest, BenchmarkResult, BenchmarkSummary, ProxyGroup};
use crossterm::event::KeyCode;

#[test]
fn single_node_benchmark_finish_does_not_flash() {
    let mut app = test_app();
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::SingleNode {
        group: "select".to_string(),
        node: "node-a".to_string(),
        result: Some(BenchmarkResult {
            name: "node-a".to_string(),
            delay: Some(42),
            completed: true,
        }),
    }))
    .expect("apply completion");

    assert_eq!(app.status, "Latency tested select / node-a: 42ms");
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
        "Sort order: LATENCY ORDER (hide failed-tested nodes, sort successful nodes by delay)"
    );
    assert!(app.flash.is_none());
}

#[test]
fn group_benchmark_finish_does_not_flash() {
    let mut app = test_app();
    app.apply_benchmark_update(BenchmarkUpdate::Finished(BenchmarkCompletion::Group {
        group: "select".to_string(),
        best: Some(BenchmarkResult {
            name: "node-a".to_string(),
            delay: Some(42),
            completed: true,
        }),
    }))
    .expect("apply completion");

    assert_eq!(app.status, "Latency tested select: best is node-a (42ms)");
    assert!(app.flash.is_none());
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
