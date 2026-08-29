use super::super::test_support::{internet_routes_app, test_app};
use crate::controller::{BenchmarkResult, BenchmarkSummary, ProxyGroup};

#[test]
fn default_panel_keeps_every_selector_member_when_probe_filter_changes() {
    let mut app = test_app();
    app.groups[0].members = vec!["hk-1".to_string(), "us-1".to_string(), "hk-2".to_string()];

    app.apply_benchmark_filter("hk".to_string())
        .expect("apply filter");

    assert_eq!(
        app.displayed_members(),
        vec!["hk-1".to_string(), "us-1".to_string(), "hk-2".to_string()]
    );
}

#[test]
fn implicit_root_mode_displays_root_choices_as_left_column() {
    let mut app = internet_routes_app();

    assert!(app.implicit_root_mode());
    assert_eq!(
        app.displayed_group_names(),
        vec!["AirTCP".to_string(), "宝贝云".to_string()]
    );

    app.groups[0].members = vec!["宝贝云".to_string()];

    assert!(app.implicit_root_mode());
    assert_eq!(app.displayed_group_names(), vec!["宝贝云".to_string()]);
}

#[test]
fn implicit_root_mode_supports_single_internet_route_selector() {
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
    app.member_index = 0;

    assert!(app.implicit_root_mode());

    app.apply_benchmark_filter("美国".to_string())
        .expect("apply filter");

    assert_eq!(app.selected_root_choice_name().as_deref(), Some("airtcp"));
    assert_eq!(
        app.displayed_members(),
        vec!["香港-a".to_string(), "美国-b".to_string()]
    );
    assert_eq!(
        app.selected_group().map(|group| group.name.as_str()),
        Some("airtcp")
    );
}

#[test]
fn implicit_root_members_follow_selected_choice() {
    let mut app = internet_routes_app();

    assert_eq!(app.selected_root_choice_name().as_deref(), Some("宝贝云"));
    assert_eq!(
        app.displayed_members(),
        vec!["bby-1".to_string(), "bby-2".to_string()]
    );

    app.internet_route_index = 0;
    app.sync_member_selection_to_current();

    assert_eq!(app.selected_root_choice_name().as_deref(), Some("AirTCP"));
    assert_eq!(
        app.displayed_members(),
        vec!["air-1".to_string(), "air-2".to_string()]
    );
    assert_eq!(app.member_index, 0);
}

#[test]
fn implicit_root_excludes_urltest_auto_choice() {
    let mut app = internet_routes_app();
    app.internet_route_index = 0;
    app.sync_member_selection_to_current();

    assert_eq!(app.selected_root_choice_name().as_deref(), Some("AirTCP"));
    assert_eq!(
        app.displayed_members(),
        vec!["air-1".to_string(), "air-2".to_string()]
    );
    assert!(app.selected_member_panel_is_manual_selector());
}

#[test]
fn implicit_root_parent_switch_targets_non_current_internet_route() {
    let app = internet_routes_app();

    assert_eq!(
        app.implicit_root_parent_switch_for_group("AirTCP"),
        Some(("手动选择".to_string(), "AirTCP".to_string()))
    );
    assert_eq!(app.implicit_root_parent_switch_for_group("宝贝云"), None);
    assert_eq!(app.implicit_root_parent_switch_for_group("自动选择"), None);
    assert_eq!(app.implicit_root_parent_switch_for_group("missing"), None);
}

#[test]
fn implicit_root_benchmark_summary_is_scoped_to_selected_choice() {
    let mut app = internet_routes_app();
    app.benchmark_workflow.set_summary(BenchmarkSummary {
        selector: "宝贝云".to_string(),
        current: Some("bby-2".to_string()),
        pattern: String::new(),
        url: "https://www.gstatic.com/generate_204".to_string(),
        timeout_ms: 5000,
        max_concurrency: 4,
        results: vec![BenchmarkResult {
            name: "bby-1".to_string(),
            delay: Some(88),
            completed: true,
        }],
    });

    assert_eq!(
        app.selected_benchmark()
            .map(|summary| summary.selector.as_str()),
        Some("宝贝云")
    );
    assert!(
        app.selected_benchmark()
            .and_then(|summary| summary.find_result("bby-1"))
            .is_some()
    );
}
