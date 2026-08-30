use super::*;
use crate::private_access::PrivateAccessRoute;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::Instant;

fn dashboard_snapshot<'a>() -> DashboardSnapshot<'a> {
    DashboardSnapshot {
        focus: Focus::Groups,
        left_pane_section: LeftPaneSection::Internet,
        internet_rows: vec![InternetRow {
            name: "select".to_string(),
            current: "node-a".to_string(),
            is_current: true,
        }],
        internet_selected: 0,
        intranet_rows: Vec::new(),
        intranet_selected: 0,
        candidate_title: "Candidates [SELECTOR ORDER]".to_string(),
        node_view_tabs: vec![
            NodeViewTab {
                label: "Current selector".to_string(),
                count: 1,
            },
            NodeViewTab {
                label: "Streaming".to_string(),
                count: 1,
            },
        ],
        active_node_view_tab: 0,
        candidate_rows: vec![CandidateRow {
            name: "node-a".to_string(),
            is_current: true,
            reachability: "3/3".to_string(),
            marker: "stable reachable".to_string(),
            compact_marker: String::new(),
            tone: CandidateTone::Success,
        }],
        candidate_selected: Some(0),
        intranet_detail: None,
        status: StatusSnapshot {
            system_proxy_enabled: false,
            tun_enabled: true,
            selection_context: "clash=rule  Pick=Manual  filter=''".to_string(),
            connections: "connections active=1 proxy=1 direct=0".to_string(),
            subscription: "subscriptions: disabled".to_string(),
            sing_box: "sing-box: managed".to_string(),
            footer: StatusFooter::Status("ready".to_string()),
        },
        flash: None,
        node_quality_detail: None,
        connections: None,
        help_index: None,
        usability_probe_diagnostics: &[],
        settings: None,
        onboarding: None,
        private_access_progress: None,
        private_access_auth: None,
    }
}

fn rendered_lines(snapshot: &DashboardSnapshot<'_>) -> Vec<String> {
    rendered_lines_at(snapshot, 110, 30)
}

fn rendered_lines_at(snapshot: &DashboardSnapshot<'_>, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| render(frame, snapshot))
        .expect("dashboard renders");
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

#[test]
fn node_view_tabs_and_candidates_remain_usable_at_normal_and_narrow_widths() {
    let snapshot = dashboard_snapshot();
    let normal = rendered_lines_at(&snapshot, 110, 30).join("\n");
    assert!(normal.contains("Current selector 1"));
    assert!(normal.contains("Streaming 1"));
    assert!(normal.contains("node-a"));
    assert!(normal.contains("3/3"));

    let narrow = rendered_lines_at(&snapshot, 64, 24).join("\n");
    assert!(narrow.contains("Current selector"));
    assert!(narrow.contains("Streaming"));
    assert!(narrow.contains("node-a"));
    assert!(narrow.contains("3/3"));
    assert!(!narrow.contains("Node details"));
}

#[test]
fn streaming_rows_adapt_without_losing_node_identity_or_reachability() {
    let mut snapshot = dashboard_snapshot();
    snapshot.active_node_view_tab = 1;
    snapshot.candidate_rows[0].is_current = false;
    snapshot.candidate_rows[0].marker = "1.0 MiB/s · 2/2 sustained · p95 80ms · cold 40ms".into();
    snapshot.candidate_rows[0].compact_marker = "1.0M/s".into();

    let wide = rendered_lines_at(&snapshot, 130, 30).join("\n");
    assert!(wide.contains("1.0 MiB/s"));
    assert!(wide.contains("2/2 sustained"));
    assert!(wide.contains("p95 80ms"));
    assert!(wide.contains("cold 40ms"));
    assert!(wide.contains("3/3"));

    snapshot.candidate_rows[0].name = "这是一个很长的中文流媒体节点名称".into();
    for width in [64, 52] {
        let narrow = rendered_lines_at(&snapshot, width, 24).join("\n");
        assert!(
            narrow.contains('这'),
            "node identity missing at width {width}"
        );
        assert!(
            narrow.contains("3/3"),
            "reachability missing at width {width}"
        );
    }
}

#[test]
fn render_consumes_a_dashboard_snapshot_without_app_state() {
    let lines = rendered_lines(&dashboard_snapshot());
    let text = lines.join("\n");

    assert!(text.contains("Internet Proxy"));
    assert!(text.contains("select"));
    assert!(text.contains("node-a"));
    assert!(text.contains("stable reachable"));
    assert!(text.contains("3/3"));
    assert!(text.contains("System Proxy: disabled"));
    assert!(text.contains("Tun Mode: enabled"));
    assert!(!text.contains("Intranet Proxy"));
    assert!(has_help_binding("\\", "Toggle TUN mode"));
}

#[test]
fn help_exposes_every_bounded_invalid_manifest_diagnostic() {
    let diagnostics = vec![
        ManifestDiagnostic {
            path: "usability-probes/bad-one.json".to_string(),
            message: "manifest id is invalid".to_string(),
        },
        ManifestDiagnostic {
            path: "usability-probes/bad-two.json".to_string(),
            message: "URL must use https".to_string(),
        },
    ];
    let mut snapshot = dashboard_snapshot();
    snapshot.help_index = Some(help_item_count(diagnostics.len()).saturating_sub(1));
    snapshot.usability_probe_diagnostics = &diagnostics;

    let lines = rendered_lines_at(&snapshot, 110, 30);
    assert!(lines.iter().all(|line| !line.chars().any(char::is_control)));
    let text = lines.join("\n");
    assert!(text.contains("invalid usability manifests"));
    assert!(text.contains("bad-one.json"));
    assert!(text.contains("manifest id is invalid"));
    assert!(text.contains("bad-two.json"));
    assert!(text.contains("URL must use https"));
}

#[test]
fn status_footer_is_rendered_below_its_box() {
    let lines = rendered_lines(&dashboard_snapshot());
    let message_row = lines
        .iter()
        .position(|line| line.contains("ready"))
        .expect("status footer row");

    assert!(message_row > 0);
    assert!(!lines[message_row].contains('─'));
    assert!(lines[message_row - 1].contains('└'));
    assert!(lines[message_row - 1].contains('┘'));
}

#[test]
fn settings_overlay_uses_typed_rows() {
    let mut snapshot = dashboard_snapshot();
    snapshot.settings = Some(SettingsPanelSnapshot {
        rows: vec![SettingRow {
            label: "Quick probe HTTPS target",
            value: "https://example.test/ping".to_string(),
        }],
        selected: 0,
        editing: None,
        error: None,
    });

    let text = rendered_lines(&snapshot).join("\n");
    assert!(text.contains("Settings"));
    assert!(text.contains("Quick probe HTTPS target"));
    assert!(text.contains("https://example.test/ping"));
}

#[test]
fn reachability_detail_renders_three_attempts_and_assessment() {
    let mut snapshot = dashboard_snapshot();
    let chart = NodeQualityDetailState {
        selector: "select".into(),
        node: "node-a".into(),
        last_refresh: Instant::now(),
        reachability_assessment: Some(NodeReachabilityAssessment {
            name: "node-a".into(),
            attempts: vec![
                ProbeOutcome::Reachable { delay_ms: 42 },
                ProbeOutcome::Timeout,
                ProbeOutcome::Reachable { delay_ms: 51 },
            ],
            assessment: Some(crate::controller::ReachabilityAssessment::Reachable),
        }),
        sustained_quality: Some(crate::sustained_quality::NodeSustainedQuality {
            name: "node-a".into(),
            outcome: crate::sustained_quality::SustainedProbeOutcome::Completed(
                crate::sustained_quality::SustainedCompletion {
                    first_byte_ms: 120,
                    completion_ms: 620,
                    bytes_read: 512 * 1024,
                    throughput_bytes_per_second: 1024 * 1024,
                },
            ),
        }),
        auto_selection_detail: Some("candidate leads; awaiting confirmation 1/2".into()),
        usability_details: Vec::new(),
        evidence_scroll: 0,
    };
    snapshot.node_quality_detail = Some(&chart);

    let text = rendered_lines(&snapshot).join("\n");
    assert!(text.contains("Reachability assessment: 2/3 reachable"));
    assert!(text.contains("Probe attempt 1: reachable (42ms)"));
    assert!(text.contains("Probe attempt 2: timeout"));
    assert!(text.contains("Probe attempt 3: reachable (51ms)"));
    assert!(text.contains("Sustained quality: 1.0 MiB/s, 524288 bytes"));
    assert!(text.contains("Automatic selection: candidate leads; awaiting confirmation 1/2"));
}

#[test]
fn intranet_detail_is_rendered_from_the_typed_profile_snapshot() {
    let mut profile = PrivateAccessProfileRuntime::default_hillstone().expect("Hillstone profile");
    profile.server = "vpn.example.com".to_string();
    profile.state = PrivateAccessState::Connected;
    profile.routes = vec![PrivateAccessRoute {
        cidr: "10.20.0.0/16".to_string(),
    }];
    profile.dns = vec!["10.20.0.53".to_string()];
    profile.domains = vec!["portal.internal.example".to_string()];
    profile.domain_suffixes = vec!["corp.example".to_string()];
    let expanded_sections = BTreeSet::new();
    let mut snapshot = dashboard_snapshot();
    snapshot.left_pane_section = LeftPaneSection::Intranet;
    snapshot.intranet_rows = vec![IntranetRow {
        id: profile.id.clone(),
        state: profile.state.clone(),
        background: false,
    }];
    snapshot.intranet_detail = Some(IntranetDetailSnapshot {
        profile: &profile,
        expanded_sections: &expanded_sections,
        scroll: 0,
        active: true,
    });

    let text = rendered_lines(&snapshot).join("\n");
    assert!(text.contains("Intranet Proxy"));
    assert!(text.contains("Intranet: hillstone"));
    assert!(text.contains("vpn.example.com:4433"));
    assert!(text.contains("10.20.0.0/16"));
    assert!(text.contains("10.20.0.53"));
    assert!(text.contains("portal.internal.example"));
    assert!(text.contains("*.corp.example"));
    assert!(text.contains("Enter expand/fold"));
}
