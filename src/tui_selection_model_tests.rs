use super::super::test_support::{internet_routes_app, test_app};
use crate::benchmark_workflow::BenchmarkUpdate;
use crate::controller::{
    BenchmarkResult, BenchmarkSummary, NodeReachabilityAssessment, ProbeOutcome, ProxyGroup,
};
use crate::sustained_quality::{NodeSustainedQuality, SustainedCompletion, SustainedProbeOutcome};
use crossterm::event::KeyCode;

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

#[test]
fn streaming_panel_filters_ranks_and_preserves_selector_members() {
    let mut app = test_app();
    app.benchmark_filter.clear();
    app.groups[0].members = vec!["node-a".into(), "node-b".into(), "node-c".into()];
    for (node, throughput) in [("node-a", 1_000), ("node-b", 2_000)] {
        app.benchmark_workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment::from_attempts(
                node.into(),
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
                name: node.into(),
                outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
                    first_byte_ms: 100,
                    completion_ms: 600,
                    bytes_read: 512 * 1024,
                    throughput_bytes_per_second: throughput,
                }),
            },
        );
    }

    assert_eq!(app.node_view_counts(), (3, 2));
    assert_eq!(app.displayed_members(), ["node-a", "node-b", "node-c"]);
    app.move_node_view_next();
    assert_eq!(app.displayed_members(), ["node-b", "node-a"]);
    app.move_node_view_previous();
    assert_eq!(app.displayed_members(), ["node-a", "node-b", "node-c"]);
}

#[test]
fn arrow_keys_switch_tabs_only_while_candidate_pane_is_focused() {
    let mut app = test_app();
    assert_eq!(
        app.node_view_panel,
        super::super::NodeViewPanel::CurrentSelector
    );

    app.handle_key(KeyCode::Right).unwrap();
    assert_eq!(app.node_view_panel, super::super::NodeViewPanel::Streaming);
    app.handle_key(KeyCode::Left).unwrap();
    assert_eq!(
        app.node_view_panel,
        super::super::NodeViewPanel::CurrentSelector
    );

    app.handle_key(KeyCode::Char('h')).unwrap();
    assert_eq!(app.focus, super::super::Focus::Groups);
    app.handle_key(KeyCode::Right).unwrap();
    assert_eq!(app.focus, super::super::Focus::Members);
    assert_eq!(
        app.node_view_panel,
        super::super::NodeViewPanel::CurrentSelector
    );
}

#[test]
fn empty_streaming_panel_makes_member_actions_safe_no_ops() {
    let mut app = test_app();
    app.move_node_view_next();
    assert!(app.displayed_members().is_empty());
    assert!(app.selected_member_name().is_none());

    for key in [
        KeyCode::Char(' '),
        KeyCode::Char('t'),
        KeyCode::Char('i'),
        KeyCode::Char('j'),
        KeyCode::Char('k'),
    ] {
        assert!(app.handle_key(key).is_ok(), "{key:?} must not exit the TUI");
        assert!(app.selected_member_name().is_none());
    }
}

#[test]
fn projection_update_resynchronizes_a_streaming_selection_that_disappears() {
    let mut app = test_app();
    app.groups[0].members = vec!["node-a".into(), "node-b".into()];
    for node in ["node-a", "node-b"] {
        app.benchmark_workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment::from_attempts(
                node.into(),
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
                name: node.into(),
                outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
                    first_byte_ms: 100,
                    completion_ms: 600,
                    bytes_read: 512 * 1024,
                    throughput_bytes_per_second: 1_000,
                }),
            },
        );
    }
    app.move_node_view_next();
    assert_eq!(app.selected_member_name().as_deref(), Some("node-a"));

    let failed = NodeSustainedQuality {
        name: "node-a".into(),
        outcome: SustainedProbeOutcome::TransferFailed {
            detail: "short body".into(),
        },
    };
    app.benchmark_workflow
        .set_sustained_quality("select", failed.clone());
    app.apply_benchmark_update(BenchmarkUpdate::SustainedProgress {
        group: "select".into(),
        result: failed,
    })
    .unwrap();

    assert_eq!(app.displayed_members(), ["node-b"]);
    assert_eq!(app.selected_member_name().as_deref(), Some("node-b"));
    app.handle_key(KeyCode::Char('j')).unwrap();
    app.handle_key(KeyCode::Char('k')).unwrap();
    assert_eq!(app.selected_member_name().as_deref(), Some("node-b"));
}
