use super::super::test_support::test_app;
use crate::auto_pick::{BackgroundLatencyResult, BackgroundLatencySnapshot};

#[test]
fn background_latency_snapshot_updates_visible_benchmark_results() {
    let mut app = test_app();

    app.apply_background_latency_snapshot(Some(&BackgroundLatencySnapshot {
        quality_generation: 0,
        selector: "select".to_string(),
        current: Some("node-a".to_string()),
        pattern: "美国".to_string(),
        url: "https://www.gstatic.com/generate_204".to_string(),
        timeout_ms: 5000,
        max_concurrency: 4,
        results: vec![
            BackgroundLatencyResult {
                name: "node-a".to_string(),
                delay: Some(88),
                completed: true,
            },
            BackgroundLatencyResult {
                name: "node-b".to_string(),
                delay: None,
                completed: true,
            },
        ],
    }));

    let summary = app.selected_benchmark().expect("benchmark summary");
    assert_eq!(
        summary.find_result("node-a").map(|result| result.delay),
        Some(Some(88))
    );
    assert_eq!(
        summary
            .find_result("node-b")
            .map(|result| result.display_delay()),
        Some("fail".to_string())
    );
}

#[test]
fn background_latency_snapshot_ignores_stale_filter_results() {
    let mut app = test_app();
    app.benchmark_filter = "new".to_string();

    app.apply_background_latency_snapshot(Some(&BackgroundLatencySnapshot {
        quality_generation: 0,
        selector: "select".to_string(),
        current: Some("node-a".to_string()),
        pattern: "old".to_string(),
        url: "https://www.gstatic.com/generate_204".to_string(),
        timeout_ms: 5000,
        max_concurrency: 4,
        results: vec![BackgroundLatencyResult {
            name: "node-a".to_string(),
            delay: Some(88),
            completed: true,
        }],
    }));

    assert!(app.selected_benchmark().is_none());
}
