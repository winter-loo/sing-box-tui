#[test]
fn pending_candidate_renders_working_shimmer_animation() {
    let mut snapshot = dashboard_snapshot();
    snapshot.candidate_rows = vec![CandidateRow {
        name: "test-node".to_string(),
        is_current: false,
        latency_signal: None,
        reachability: String::new(),
        compact_marker: "checking TCP 22 (2s)".to_string(),
        marker: "• checking TCP 22 (2s)".to_string(),
        tone: CandidateTone::Pending,
    }];
    snapshot.pending_animation_tick = 3;

    let lines = rendered_lines(&snapshot);
    let text = lines.join("
");
    assert!(text.contains("test-node"));
    assert!(text.contains("checking TCP 22 (2s)"));
}

#[test]
fn active_usability_probe_renders_braille_spinner_instead_of_tab_count() {
    let mut snapshot = dashboard_snapshot();
    snapshot.node_view_tabs[1].spinner = Some("⠋".to_string());

    let rendered = rendered_lines(&snapshot).join("
");
    assert!(rendered.contains("Streaming ⠋"));
    assert!(!rendered.contains("Streaming 1"));
}

use super::*;
use crate::private_access::PrivateAccessRoute;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::Instant;

#[test]
fn pending_candidate_animation_has_distinct_bright_and_dim_frames() {
    let bright = pending_candidate_style(true);
    let dim = pending_candidate_style(false);

    assert_eq!(bright.fg, Some(Color::LightYellow));
    assert_eq!(dim.fg, Some(Color::DarkGray));
    assert_ne!(bright, dim);
}

#[test]
fn latency_signal_uses_the_requested_color_thresholds() {
    assert_eq!(
        latency_signal_style(LatencySignalState::Untested).fg,
        Some(Color::DarkGray)
    );
    assert_eq!(
        latency_signal_style(LatencySignalState::Reachable { delay_ms: 199 }).fg,
        Some(Color::Green)
    );
    assert_eq!(
        latency_signal_style(LatencySignalState::Reachable { delay_ms: 200 }).fg,
        Some(Color::Yellow)
    );
    assert_eq!(
        latency_signal_style(LatencySignalState::Reachable { delay_ms: 400 }).fg,
        Some(Color::Rgb(184, 134, 11))
    );
    assert_eq!(
        latency_signal_style(LatencySignalState::Reachable { delay_ms: 600 }).fg,
        Some(Color::Rgb(205, 92, 92))
    );
    let unreachable = latency_signal_style(LatencySignalState::Unreachable);
    assert_eq!(unreachable.fg, Some(Color::Rgb(139, 0, 0)));
    assert!(unreachable.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn braille_signal_uses_thin_half_cell_bars_with_fixed_spacing() {
    let levels = [2, 5, 8]
        .map(latency_signal_glyph)
        .into_iter()
        .collect::<String>();
    assert_eq!(levels, "⡀⡄⡆");
    assert_eq!(unicode_width::UnicodeWidthStr::width(levels.as_str()), 3);

    let three_bars = [8, 4, 2]
        .map(latency_signal_glyph)
        .into_iter()
        .collect::<String>();
    assert_eq!(three_bars, "⡆⡄⡀");
    assert_eq!(unicode_width::UnicodeWidthStr::width(three_bars.as_str()), 3);
}

fn dashboard_snapshot<'a>() -> DashboardSnapshot<'a> {
    DashboardSnapshot {
        operational_workspace: crate::tui_state::OperationalWorkspace::Internet,
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
        candidate_notice: None,
        node_view_tabs: vec![
            NodeViewTab {
                label: "Current selector".to_string(),
                count: 1,
                spinner: None,
            },
            NodeViewTab {
                label: "Streaming".to_string(),
                count: 1,
                spinner: None,
            },
        ],
        active_node_view_tab: 0,
        candidate_rows: vec![CandidateRow {
            name: "node-a".to_string(),
            is_current: true,
            latency_signal: Some(LatencySignal {
                bars: [
                    LatencySignalBar {
                        height: 8,
                        state: LatencySignalState::Reachable { delay_ms: 100 },
                    },
                    LatencySignalBar {
                        height: 4,
                        state: LatencySignalState::Reachable { delay_ms: 200 },
                    },
                    LatencySignalBar {
                        height: 2,
                        state: LatencySignalState::Reachable { delay_ms: 400 },
                    },
                ],
                average_ms: Some(233),
            }),
            reachability: String::new(),
            marker: String::new(),
            compact_marker: String::new(),
            tone: CandidateTone::Success,
        }],
        candidate_selected: Some(0),
        pending_animation_tick: 0,
        pending_animation_bright: true,
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

#[test]
fn custom_probe_error_is_fully_visible_inside_the_candidate_panel() {
    let mut snapshot = dashboard_snapshot();
    snapshot.candidate_title = "Candidates for select [LOW LATENCY] · FAILED".into();
    snapshot.candidate_notice = Some(CandidateNotice {
        title: "Probe error".into(),
        message: "run #7 failed: the beginning explains the failure and the complete diagnostic ends with UTF8_SENTINEL".into(),
        error: true,
    });

    let rendered = rendered_lines_at(&snapshot, 72, 24).join("\n");
    assert!(rendered.contains("Probe error"));
    assert!(rendered.contains("the beginning explains"));
    assert!(rendered.contains("UTF8_SENTINEL"));
}

#[test]
fn custom_probe_progress_is_visible_before_any_node_result() {
    let mut snapshot = dashboard_snapshot();
    snapshot.candidate_rows.clear();
    snapshot.candidate_title = "Candidates for select [LOW LATENCY]".into();
    snapshot.candidate_notice = Some(CandidateNotice {
        title: "Probe progress".into(),
        message: "HTTPS 44/108 · TCP 22 3/17 · accepted 2\nScanning node US 01...".into(),
        error: false,
    });

    let rendered = rendered_lines_at(&snapshot, 72, 24).join("\n");
    assert!(rendered.contains("Probe progress"));
    assert!(rendered.contains("HTTPS 44/108 · TCP 22 3/17 · accepted 2"));
    assert!(rendered.contains("Scanning node US 01"));
    let title_line = rendered
        .lines()
        .find(|line| line.contains("Candidates for select"))
        .expect("candidate title");
    assert!(!title_line.contains("HTTPS"));
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
    assert!(normal.contains("⡆⡄⡀"));
    assert!(normal.contains("233ms"));
    assert!(!normal.contains("avg"));

    let narrow = rendered_lines_at(&snapshot, 64, 24).join("\n");
    assert!(narrow.contains("Current selector"));
    assert!(narrow.contains("Streaming"));
    assert!(narrow.contains("node-a"));
    assert!(narrow.contains("⡆⡄⡀"));
    assert!(narrow.contains("233ms"));
    assert!(!narrow.contains("avg"));
    assert!(!narrow.contains("Node details"));
}

#[test]
fn streaming_rows_adapt_without_losing_node_identity_or_reachability() {
    let mut snapshot = dashboard_snapshot();
    snapshot.active_node_view_tab = 1;
    snapshot.candidate_rows[0].is_current = false;
    snapshot.candidate_rows[0].latency_signal = None;
    snapshot.candidate_rows[0].reachability = String::new();
    snapshot.candidate_rows[0].marker = "1.0 MiB/s".into();
    snapshot.candidate_rows[0].compact_marker = "1.0M/s".into();

    let wide = rendered_lines_at(&snapshot, 130, 30).join("
");
    assert!(wide.contains("1.0 MiB/s"));

    snapshot.candidate_rows[0].name = "这是一个很长的中文流媒体节点名称".into();
    for width in [64, 52] {
        let narrow = rendered_lines_at(&snapshot, width, 24).join("
");
        assert!(
            narrow.contains("这"),
            "node identity missing at width {width}"
        );
    }
}

#[test]
fn render_consumes_a_dashboard_snapshot_without_app_state() {
    let lines = rendered_lines(&dashboard_snapshot());
    let text = lines.join("\n");

    assert!(text.contains("node-a"));
    assert!(text.contains("⡆⡄⡀"));
    assert!(text.contains("233ms"));
    assert!(!text.contains("avg"));
    assert!(!text.contains("stable reachable"));
    assert!(!text.contains("3/3"));
    assert!(text.contains("dashboard"));
    assert!(text.contains("connections"));
    assert!(text.contains("quality"));
    assert!(text.contains("settings"));
    assert!(text.contains("help"));
    assert!(text.contains("provider"));
    assert!(text.contains("GLOBAL NET"));
    assert!(text.contains("STABLE"));
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

    assert_eq!(message_row, 29);
    assert!(!lines[message_row].contains('─'));
    assert!(!lines[message_row - 1].contains('└'));
    assert!(!lines[message_row - 1].contains('┘'));
    assert!(lines[message_row].contains("GLOBAL NET"));
    assert!(lines[message_row].contains("STABLE"));
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
        quick_history: crate::storage::NodeQuickHistory {
            successful_rounds: 3,
            rounds: 4,
            warm_median_ms: Some(45),
            p95_ms: Some(80),
            cold_start_ms: Some(60),
        },
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

#[test]
fn reachability_badge_style_matches_four_level_design_system_tokens() {
    let theme = Theme::detect();
    assert_eq!(
        reachability_badge_style("Stable", CandidateTone::Success, &theme),
        theme.style_success()
    );
    assert_eq!(
        reachability_badge_style("stable reachable", CandidateTone::Success, &theme),
        theme.style_success()
    );
    assert_eq!(
        reachability_badge_style("Reachable", CandidateTone::Success, &theme),
        theme.style_breadcrumb()
    );
    assert_eq!(
        reachability_badge_style("2/3 reachable", CandidateTone::Success, &theme),
        theme.style_breadcrumb()
    );
    assert_eq!(
        reachability_badge_style("Degraded", CandidateTone::Error, &theme),
        theme.style_warning()
    );
    assert_eq!(
        reachability_badge_style("1/3 degraded", CandidateTone::Error, &theme),
        theme.style_warning()
    );
    assert_eq!(
        reachability_badge_style("Unreachable", CandidateTone::Error, &theme),
        theme.style_danger()
    );
    assert_eq!(
        reachability_badge_style("0/3 unreachable", CandidateTone::Error, &theme),
        theme.style_danger()
    );
    assert_eq!(
        reachability_badge_style("Error", CandidateTone::Error, &theme),
        theme.style_danger()
    );
}

#[test]
fn candidate_row_renders_four_level_reachability_assessment_badges() {
    let mut snapshot = dashboard_snapshot();
    snapshot.candidate_rows = vec![
        CandidateRow {
            name: "tokyo-01".to_string(),
            is_current: false,
            latency_signal: None,
            reachability: "Stable".to_string(),
            marker: String::new(),
            compact_marker: String::new(),
            tone: CandidateTone::Success,
        },
        CandidateRow {
            name: "osaka-02".to_string(),
            is_current: false,
            latency_signal: None,
            reachability: "Reachable".to_string(),
            marker: String::new(),
            compact_marker: String::new(),
            tone: CandidateTone::Success,
        },
        CandidateRow {
            name: "seoul-03".to_string(),
            is_current: false,
            latency_signal: None,
            reachability: "Degraded".to_string(),
            marker: String::new(),
            compact_marker: String::new(),
            tone: CandidateTone::Error,
        },
        CandidateRow {
            name: "london-04".to_string(),
            is_current: false,
            latency_signal: None,
            reachability: "Unreachable".to_string(),
            marker: String::new(),
            compact_marker: String::new(),
            tone: CandidateTone::Error,
        },
    ];

    let rendered = rendered_lines(&snapshot).join("\n");
    assert!(rendered.contains("tokyo-01"));
    assert!(rendered.contains("Stable"));
    assert!(rendered.contains("osaka-02"));
    assert!(rendered.contains("Reachable"));
    assert!(rendered.contains("seoul-03"));
    assert!(rendered.contains("Degraded"));
    assert!(rendered.contains("london-04"));
    assert!(rendered.contains("Unreachable"));
}

#[test]
fn candidate_row_cjk_grapheme_truncation_preserves_multibyte_safety() {
    let mut snapshot = dashboard_snapshot();
    snapshot.candidate_rows = vec![CandidateRow {
        name: "🌸东京直连专线节点UltraLongName".to_string(),
        is_current: false,
        latency_signal: Some(LatencySignal {
            bars: [
                LatencySignalBar {
                    height: 8,
                    state: LatencySignalState::Reachable { delay_ms: 50 },
                },
                LatencySignalBar {
                    height: 8,
                    state: LatencySignalState::Reachable { delay_ms: 50 },
                },
                LatencySignalBar {
                    height: 8,
                    state: LatencySignalState::Reachable { delay_ms: 50 },
                },
            ],
            average_ms: Some(50),
        }),
        reachability: "Stable".to_string(),
        marker: String::new(),
        compact_marker: String::new(),
        tone: CandidateTone::Success,
    }];

    let narrow = rendered_lines_at(&snapshot, 60, 24).join("\n");
    assert!(narrow.contains("🌸"));
    assert!(narrow.contains("50ms"));
    assert!(narrow.contains("Stable"));
}

#[test]
fn candidate_row_renders_cold_start_ms_and_sustained_throughput_speed() {
    let mut snapshot = dashboard_snapshot();
    snapshot.candidate_rows = vec![
        CandidateRow {
            name: "streaming-node".to_string(),
            is_current: false,
            latency_signal: None,
            reachability: String::new(),
            marker: "24.5 MiB/s".to_string(),
            compact_marker: "24.5M/s".to_string(),
            tone: CandidateTone::Success,
        },
        CandidateRow {
            name: "cold-start-node".to_string(),
            is_current: false,
            latency_signal: None,
            reachability: String::new(),
            marker: "cold start 62ms".to_string(),
            compact_marker: "62ms".to_string(),
            tone: CandidateTone::Success,
        },
    ];

    let wide = rendered_lines_at(&snapshot, 120, 30).join("\n");
    assert!(wide.contains("streaming-node"));
    assert!(wide.contains("24.5 MiB/s"));
    assert!(wide.contains("cold-start-node"));
    assert!(wide.contains("cold start 62ms"));
}

#[test]
fn candidate_row_selected_focus_styling_matches_figma_accent() {
    let theme = Theme::detect();
    let focused = theme.style_focused_row();
    let breadcrumb = theme.style_breadcrumb();

    assert_eq!(focused.bg, Some(theme.bg_selected()));
    assert_eq!(focused.fg, Some(theme.text_accent()));
    assert_eq!(breadcrumb.fg, Some(theme.text_accent()));
    assert!(focused.add_modifier.contains(Modifier::BOLD));
    assert!(breadcrumb.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn usability_tabs_retain_current_selector_streaming_and_custom_views() {
    let mut snapshot = dashboard_snapshot();
    snapshot.node_view_tabs = vec![
        NodeViewTab {
            label: "Current selector".to_string(),
            count: 5,
            spinner: None,
        },
        NodeViewTab {
            label: "Streaming".to_string(),
            count: 3,
            spinner: None,
        },
        NodeViewTab {
            label: "Custom Gemini".to_string(),
            count: 2,
            spinner: None,
        },
    ];
    snapshot.active_node_view_tab = 2;

    let text = rendered_lines_at(&snapshot, 120, 30).join("\n");
    assert!(text.contains("Current selector 5"));
    assert!(text.contains("Streaming 3"));
    assert!(text.contains("Custom Gemini 2"));
    assert!(text.contains("│"));
}

#[test]
fn internet_workspace_120x30_matches_figma_8_2_borderless_specification() {
    let mut snapshot = dashboard_snapshot();
    snapshot.node_view_tabs = vec![
        NodeViewTab {
            label: "All".to_string(),
            count: 18,
            spinner: None,
        },
        NodeViewTab {
            label: "Streaming".to_string(),
            count: 12,
            spinner: None,
        },
        NodeViewTab {
            label: "GitHub SSH".to_string(),
            count: 5,
            spinner: None,
        },
        NodeViewTab {
            label: "Agy Gemini".to_string(),
            count: 2,
            spinner: None,
        },
        NodeViewTab {
            label: "GitHub Web".to_string(),
            count: 17,
            spinner: None,
        },
    ];
    snapshot.candidate_rows = vec![
        CandidateRow {
            name: "JP-Edge-03".to_string(),
            is_current: true,
            latency_signal: Some(LatencySignal {
                bars: [
                    LatencySignalBar {
                        height: 8,
                        state: LatencySignalState::Reachable { delay_ms: 42 },
                    },
                    LatencySignalBar {
                        height: 5,
                        state: LatencySignalState::Reachable { delay_ms: 42 },
                    },
                    LatencySignalBar {
                        height: 2,
                        state: LatencySignalState::Reachable { delay_ms: 42 },
                    },
                ],
                average_ms: Some(42),
            }),
            reachability: "Stable".to_string(),
            marker: "8.0 MiB/s".to_string(),
            compact_marker: "8.0M/s".to_string(),
            tone: CandidateTone::Success,
        },
        CandidateRow {
            name: "SG-Transit-07".to_string(),
            is_current: false,
            latency_signal: Some(LatencySignal {
                bars: [
                    LatencySignalBar {
                        height: 8,
                        state: LatencySignalState::Reachable { delay_ms: 68 },
                    },
                    LatencySignalBar {
                        height: 5,
                        state: LatencySignalState::Reachable { delay_ms: 68 },
                    },
                    LatencySignalBar {
                        height: 2,
                        state: LatencySignalState::Reachable { delay_ms: 68 },
                    },
                ],
                average_ms: Some(68),
            }),
            reachability: "Stable".to_string(),
            marker: String::new(),
            compact_marker: String::new(),
            tone: CandidateTone::Success,
        },
        CandidateRow {
            name: "JP-Backup-01".to_string(),
            is_current: false,
            latency_signal: Some(LatencySignal {
                bars: [
                    LatencySignalBar {
                        height: 8,
                        state: LatencySignalState::Reachable { delay_ms: 63 },
                    },
                    LatencySignalBar {
                        height: 5,
                        state: LatencySignalState::Reachable { delay_ms: 63 },
                    },
                    LatencySignalBar {
                        height: 2,
                        state: LatencySignalState::Reachable { delay_ms: 63 },
                    },
                ],
                average_ms: Some(63),
            }),
            reachability: "Stable".to_string(),
            marker: String::new(),
            compact_marker: String::new(),
            tone: CandidateTone::Success,
        },
    ];

    let lines = rendered_lines_at(&snapshot, 120, 30);
    assert_eq!(lines.len(), 30);

    // Row 0 of body: Filter Tabs (borderless)
    let tabs_row = &lines[0];
    assert!(tabs_row.contains("All 18"));
    assert!(tabs_row.contains("Streaming 12"));
    assert!(tabs_row.contains("GitHub SSH 5"));
    assert!(tabs_row.contains("Agy Gemini 2"));
    assert!(tabs_row.contains("GitHub Web 17"));
    assert!(tabs_row.contains("│"));
    assert!(!tabs_row.contains("┌"));
    assert!(!tabs_row.contains("Node views"));

    // Candidate rows start at Row 1, full 120-column width without sidebar or box borders
    let row1 = &lines[1];
    assert!(row1.contains("* JP-Edge-03"));
    assert!(row1.contains("8.0 MiB/s"));
    assert!(row1.contains("Stable"));
    assert!(row1.contains("⡆⡄⡀"));
    assert!(row1.contains("42ms"));
    assert!(!row1.contains("│")); // No 26-column sidebar divider!

    let row2 = &lines[2];
    assert!(row2.contains("SG-Transit-07"));
    assert!(row2.contains("68ms"));
    assert!(!row2.contains("│"));

    let row3 = &lines[3];
    assert!(row3.contains("JP-Backup-01"));
    assert!(row3.contains("63ms"));
    assert!(!row3.contains("│"));

    // Bottom row (Row 29): Integrated Footer
    let footer_row = &lines[29];
    assert!(footer_row.contains("dashboard"));
    assert!(footer_row.contains("connections"));
    assert!(footer_row.contains("quality"));
    assert!(footer_row.contains("settings"));
    assert!(footer_row.contains("help"));
    assert!(footer_row.contains("provider"));
    assert!(footer_row.contains("Intranet →"));
    assert!(footer_row.contains("GLOBAL NET"));
    assert!(footer_row.contains("STABLE"));

    // Verify no legacy box borders anywhere in the 30 rows
    assert!(!lines.iter().any(|line| line.contains("Internet Proxy")));
    assert!(!lines.iter().any(|line| line.contains("Status") && line.contains("─")));
}

fn rendered_provider_modal_lines_at(
    providers: &[ProviderItem],
    selected_index: usize,
    width: u16,
    height: u16,
) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let theme = Theme::detect();
    terminal
        .draw(|frame| {
            render_provider_modal(frame, frame.area(), &theme, providers, selected_index)
        })
        .expect("provider modal renders");
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

#[test]
fn provider_modal_renders_centered_dialog_with_providers() {
    let providers = vec![
        ProviderItem {
            name: "AirTCP".to_string(),
            is_current: true,
            node_count: 18,
        },
        ProviderItem {
            name: "宝贝云".to_string(),
            is_current: false,
            node_count: 12,
        },
    ];

    let lines = rendered_provider_modal_lines_at(&providers, 1, 120, 30);
    let text = lines.join("\n");
    assert!(text.contains("INTERNET PROXY PROVIDER (p)"));
    assert!(text.contains("AirTCP"));
    assert!(text.contains("* CURRENT"));
    assert!(text.contains("(18)"));
    assert!(text.contains("> 宝") && text.contains("云"));
    assert!(text.contains("(12)"));
    assert!(text.contains("[Enter]"));
    assert!(text.contains("Confirm"));
    assert!(text.contains("[Esc]"));
    assert!(text.contains("Close"));
}

