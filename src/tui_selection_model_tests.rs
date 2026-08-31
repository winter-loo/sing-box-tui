use super::super::test_support::{internet_routes_app, test_app};
use super::live_usability_members;
use crate::automatic_selection::{NodeViewId, RankingPolicy};
use crate::benchmark_workflow::BenchmarkUpdate;
use crate::controller::{NodeReachabilityAssessment, ProbeOutcome, ProxyGroup};
use crate::storage::{StoredUsabilityProbeRun, UsabilityProbeFactRecord};
use crate::sustained_quality::{NodeSustainedQuality, SustainedCompletion, SustainedProbeOutcome};
use crate::usability_probe::{UsabilityProbeManifest, UsabilityProbeSource};
use crossterm::event::KeyCode;
use std::path::PathBuf;

#[test]
fn live_usability_members_keep_pending_and_accepted_but_remove_rejected() {
    let members = ["accepted", "rejected", "checking", "untested"].map(str::to_string);
    let results = [
        (
            "accepted".to_string(),
            crate::usability_probe::UsabilityProbeNodeResult {
                node: "accepted".to_string(),
                usable: true,
                detail: None,
            },
        ),
        (
            "rejected".to_string(),
            crate::usability_probe::UsabilityProbeNodeResult {
                node: "rejected".to_string(),
                usable: false,
                detail: None,
            },
        ),
    ]
    .into_iter()
    .collect();

    assert_eq!(
        live_usability_members(&members, Some("checking"), &results),
        ["accepted", "checking"]
    );
    assert_eq!(
        live_usability_members(&members, None, &results),
        ["accepted"]
    );
}

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
fn implicit_root_quality_evidence_is_scoped_to_selected_choice() {
    let mut app = internet_routes_app();
    let assessment = NodeReachabilityAssessment::from_attempts(
        "bby-1".to_string(),
        vec![
            ProbeOutcome::Reachable { delay_ms: 80 },
            ProbeOutcome::Reachable { delay_ms: 88 },
            ProbeOutcome::Reachable { delay_ms: 92 },
        ],
    );
    app.benchmark_workflow
        .set_reachability_assessment("宝贝云", assessment.clone());

    assert_eq!(
        app.benchmark_workflow
            .reachability_assessment("宝贝云", "bby-1"),
        Some(&assessment)
    );
    assert!(
        app.benchmark_workflow
            .reachability_assessment("AirTCP", "bby-1")
            .is_none()
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
fn discovered_custom_tab_starts_untested_then_projects_only_current_selector_members() {
    let mut app = test_app();
    app.benchmark_filter.clear();
    app.groups[0].members = vec!["node-a".into(), "node-b".into()];
    app.usability_probe_manifests.push(UsabilityProbeManifest {
        id: NodeViewId::new("github-web").unwrap(),
        label: "GitHub Web".to_string(),
        ranking_policy: RankingPolicy::LowLatency,
        source: UsabilityProbeSource::Url("https://github.com/404-is-still-http".to_string()),
        background: false,
        interval: None,
        result_ttl: None,
        timeout: std::time::Duration::from_secs(600),
        source_path: PathBuf::from("github-web.json"),
    });

    app.move_node_view_next();
    app.move_node_view_next();
    assert_eq!(
        app.node_view_panel,
        super::super::NodeViewPanel::Custom(NodeViewId::new("github-web").unwrap())
    );
    assert!(app.displayed_members().is_empty());
    {
        let snapshot = app.view_snapshot();
        assert_eq!(snapshot.active_node_view_tab, 2);
        assert_eq!(snapshot.node_view_tabs[2].label, "GitHub Web");
        assert_eq!(snapshot.node_view_tabs[2].count, 0);
        assert!(snapshot.candidate_rows.is_empty());
        assert!(snapshot.candidate_title.contains("UNTESTED"));
    }

    app.usability_probe_projection_cache.insert(
        (NodeViewId::new("github-web").unwrap(), "select".to_string()),
        StoredUsabilityProbeRun {
            run_id: 7,
            completed_at_ms: 42,
            expires_at_ms: None,
            summary: Some("complete".to_string()),
            latest_attempt: None,
            results: vec![
                UsabilityProbeFactRecord {
                    node: "outside-selector".to_string(),
                    usable: true,
                    detail: Some("must stay hidden".to_string()),
                },
                UsabilityProbeFactRecord {
                    node: "node-a".to_string(),
                    usable: false,
                    detail: Some("application rejected".to_string()),
                },
                UsabilityProbeFactRecord {
                    node: "node-b".to_string(),
                    usable: true,
                    detail: Some("valid HTTP response".to_string()),
                },
            ],
        },
    );
    assert_eq!(app.displayed_members(), ["node-b"]);
    {
        let snapshot = app.view_snapshot();
        assert_eq!(snapshot.node_view_tabs[2].count, 1);
        assert_eq!(snapshot.candidate_rows.len(), 1);
        assert_eq!(snapshot.candidate_rows[0].name, "node-b");
        assert_eq!(snapshot.candidate_rows[0].marker, "valid HTTP response");
        assert!(snapshot.candidate_title.contains("LOW LATENCY"));
    }

    app.handle_key(KeyCode::Char('U'))
        .expect("manual probe action remains in the TUI");
    assert!(app.status.contains("Cannot run GitHub Web usability probe"));
    app.move_node_view_previous();
    assert_eq!(app.node_view_panel, super::super::NodeViewPanel::Streaming);
}

#[test]
fn custom_panel_identity_survives_manifest_reordering_and_missing_files() {
    let mut app = test_app();
    app.benchmark_filter.clear();
    let manifest = |id: &str, label: &str| UsabilityProbeManifest {
        id: NodeViewId::new(id).unwrap(),
        label: label.to_string(),
        ranking_policy: RankingPolicy::Balanced,
        source: UsabilityProbeSource::Url("https://example.test/".to_string()),
        background: false,
        interval: None,
        result_ttl: None,
        timeout: std::time::Duration::from_secs(600),
        source_path: PathBuf::from(format!("{id}.json")),
    };
    app.usability_probe_manifests = vec![manifest("alpha", "Alpha"), manifest("beta", "Beta")];
    let beta = NodeViewId::new("beta").unwrap();
    app.node_view_panel = super::super::NodeViewPanel::Custom(beta.clone());
    app.auto_select_node_view = beta.clone();

    app.usability_probe_manifests.swap(0, 1);
    {
        let snapshot = app.view_snapshot();
        assert_eq!(snapshot.active_node_view_tab, 2);
        assert_eq!(snapshot.node_view_tabs[2].label, "Beta");
    }
    assert_eq!(
        app.runtime_state().active_node_view.as_ref(),
        Some(&beta),
        "manifest order is presentation state, never persisted identity"
    );

    app.usability_probe_manifests.clear();
    assert!(app.displayed_members().is_empty());
    let snapshot = app.view_snapshot();
    assert_eq!(snapshot.active_node_view_tab, 2);
    assert_eq!(snapshot.node_view_tabs[2].label, "Unavailable (beta)");
    assert_eq!(snapshot.node_view_tabs[2].count, 0);
    assert_eq!(app.auto_select_node_view, beta);
}

#[test]
fn custom_snapshot_and_keyboard_navigation_share_selector_order() {
    let mut app = test_app();
    app.benchmark_filter.clear();
    app.groups[0].members = vec!["z-node".into(), "a-node".into(), "m-node".into()];
    app.usability_probe_manifests.push(UsabilityProbeManifest {
        id: NodeViewId::new("ordered").unwrap(),
        label: "Ordered".to_string(),
        ranking_policy: RankingPolicy::Balanced,
        source: UsabilityProbeSource::Url("https://example.test/".to_string()),
        background: false,
        interval: None,
        result_ttl: None,
        timeout: std::time::Duration::from_secs(600),
        source_path: PathBuf::from("ordered.json"),
    });
    app.usability_probe_projection_cache.insert(
        (NodeViewId::new("ordered").unwrap(), "select".to_string()),
        StoredUsabilityProbeRun {
            run_id: 8,
            completed_at_ms: 43,
            expires_at_ms: None,
            summary: None,
            latest_attempt: None,
            // Persistence order is deliberately lexical and must never control the TUI rows.
            results: ["a-node", "m-node", "z-node"]
                .into_iter()
                .map(|node| UsabilityProbeFactRecord {
                    node: node.to_string(),
                    usable: true,
                    detail: None,
                })
                .collect(),
        },
    );
    app.move_node_view_next();
    app.move_node_view_next();

    let queries_before = app
        .benchmark_workflow
        .usability_projection_query_count_for_test();
    for _ in 0..3 {
        let _ = app.view_snapshot();
    }
    assert_eq!(
        app.benchmark_workflow
            .usability_projection_query_count_for_test(),
        queries_before,
        "repeated dashboard snapshots must remain storage-I/O free"
    );

    assert_eq!(app.displayed_members(), ["z-node", "a-node", "m-node"]);
    {
        let snapshot = app.view_snapshot();
        assert_eq!(
            snapshot
                .candidate_rows
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["z-node", "a-node", "m-node"]
        );
        assert_eq!(snapshot.candidate_selected, Some(0));
    }

    app.handle_key(KeyCode::Char('j')).expect("move down");
    assert_eq!(app.selected_member_name().as_deref(), Some("a-node"));
    assert_eq!(app.view_snapshot().candidate_selected, Some(1));
    app.handle_key(KeyCode::Char('k')).expect("move up");
    assert_eq!(app.selected_member_name().as_deref(), Some("z-node"));
    assert_eq!(app.view_snapshot().candidate_selected, Some(0));
}

#[test]
fn expired_custom_results_remain_visible_but_cannot_enter_candidates() {
    let mut app = test_app();
    app.benchmark_filter.clear();
    let id = NodeViewId::new("expiring").unwrap();
    app.usability_probe_manifests.push(UsabilityProbeManifest {
        id: id.clone(),
        label: "Expiring".to_string(),
        ranking_policy: RankingPolicy::Balanced,
        source: UsabilityProbeSource::Url("https://example.test/".to_string()),
        background: true,
        interval: Some(std::time::Duration::from_secs(60)),
        result_ttl: Some(std::time::Duration::from_secs(1)),
        timeout: std::time::Duration::from_secs(30),
        source_path: PathBuf::from("expiring.json"),
    });
    app.usability_probe_projection_cache.insert(
        (id.clone(), "select".to_string()),
        StoredUsabilityProbeRun {
            run_id: 12,
            completed_at_ms: 1,
            expires_at_ms: Some(2),
            summary: Some("previously accepted".to_string()),
            results: vec![UsabilityProbeFactRecord {
                node: "node-a".to_string(),
                usable: true,
                detail: Some("accepted before expiry".to_string()),
            }],
            latest_attempt: None,
        },
    );
    app.node_view_panel = super::super::NodeViewPanel::Custom(id.clone());

    let run = app
        .custom_usability_run(&id, "select", &app.groups[0].members)
        .expect("expired evidence remains inspectable");
    assert!(app.custom_usability_run_is_expired(&run));
    let snapshot = app.view_snapshot();
    assert!(snapshot.candidate_rows.is_empty());
    assert!(snapshot.candidate_title.contains("EXPIRED"));
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
