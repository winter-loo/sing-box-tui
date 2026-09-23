use crate::tui::ds::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::{
    ActiveView, App, IntranetDetailSection, OperationalWorkspace, PrivateAccessAuthModal,
    PrivateAccessProgressEntry, PrivateAccessProgressModal, PrivateAccessProgressTone,
    PrivateAccessState, ProxyGroup, draw, test_support, view::OnboardingState,
};
use crate::private_access::PrivateAccessAuthField;
use crate::private_access_session::PrivateAccessProfileRuntime;

// The terminal contract starts at 80x24. Exercise both independent compact
// dimensions, the 120x30 design reference, and non-reference aspect ratios.
const VIEWPORTS: &[(u16, u16)] = &[
    (80, 24),  // minimum
    (96, 27),  // intermediate
    (120, 30), // Figma reference
    (160, 30), // wide
    (120, 44), // tall
    (107, 35), // non-standard aspect ratio
    (106, 32), // observed Windows Terminal window
];

fn rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn shipped_workspaces_keep_navigation_and_status_at_viewport_edges() {
    for &(width, height) in VIEWPORTS {
        let mut app = test_support::test_app();
        let internet = rows(&mut app, width, height);
        assert!(
            internet[0].contains("INTERNET / select / node-a"),
            "{width}x{height}"
        );
        assert!(
            internet[height as usize - 1].contains("GLOBAL NET"),
            "{width}x{height}"
        );
        assert!(
            !internet[height as usize - 1].contains("[Tab] Intranet"),
            "{width}x{height} advertises a workspace shortcut that Tab does not perform"
        );
        assert!(
            internet[1..height as usize - 1]
                .iter()
                .all(|row| !row.contains("GLOBAL NET"))
        );
        assert!(
            internet[height as usize - 1]
                .trim_end()
                .ends_with("↑0.0M/s")
        );

        app.set_operational_workspace(OperationalWorkspace::PrivateAccess)
            .unwrap();
        let intranet = rows(&mut app, width, height);
        assert!(intranet[0].contains("INTRANET"), "{width}x{height}");
        assert!(
            intranet[height as usize - 1].contains("GLOBAL NET"),
            "{width}x{height}"
        );
        assert!(
            !intranet[height as usize - 1].contains("o settings   o settings"),
            "{width}x{height} repeats the only available action"
        );
        assert!(
            intranet[1..height as usize - 1]
                .iter()
                .all(|row| !row.contains("GLOBAL NET"))
        );
        assert!(
            intranet[height as usize - 1]
                .trim_end()
                .ends_with("↑0.0M/s")
        );

        app.active_view = ActiveView::NodeDashboard;
        let dashboard = rows(&mut app, width, height);
        assert!(dashboard[0].contains("NODE DASHBOARD"), "{width}x{height}");
        assert!(
            dashboard[height as usize - 1].contains("GLOBAL NET"),
            "{width}x{height}"
        );
        assert!(
            dashboard[1..height as usize - 1]
                .iter()
                .all(|row| !row.contains("GLOBAL NET"))
        );
        assert!(
            dashboard[height as usize - 1]
                .trim_end()
                .ends_with("↑0.0M/s")
        );
    }
}

#[test]
fn global_dialogs_keep_their_controls_and_status_visible_at_supported_sizes() {
    for &(width, height) in VIEWPORTS {
        let mut app = test_support::test_app();
        let cases: &[(&str, fn(&mut App))] = &[
            ("ACTIVE CONNECTIONS", |app| app.show_connections = true),
            ("NODE QUALITY", |app| {
                app.open_node_quality_detail().unwrap()
            }),
            ("SETTINGS", |app| app.open_settings_panel()),
            ("KEYBINDINGS", |app| app.open_help_panel()),
            ("COMMANDS", |app| app.toggle_command_palette()),
            ("INTERNET PROXY PROVIDER", |app| app.open_provider_modal()),
        ];
        for &(title, setup) in cases {
            app.show_connections = false;
            app.node_quality_detail = None;
            app.show_settings = false;
            app.show_help = false;
            app.command_palette = None;
            app.provider_modal = None;
            setup(&mut app);
            let rendered = rows(&mut app, width, height);
            let text = rendered.join("\n");
            assert!(text.contains(title), "{title} at {width}x{height}");
            assert!(
                text.contains("Esc"),
                "{title} has no close control at {width}x{height}"
            );
            assert!(
                rendered[height as usize - 1].contains("GLOBAL NET"),
                "{title} status at {width}x{height}"
            );
            assert!(
                rendered[height as usize - 1]
                    .chars()
                    .skip(width as usize / 2)
                    .collect::<String>()
                    .contains("GLOBAL NET"),
                "{title} puts status outside the right half at {width}x{height}"
            );
            if title == "COMMANDS" {
                assert!(
                    !rendered[height as usize - 1].contains("provider"),
                    "command palette leaks page shortcuts at {width}x{height}"
                );
            }
            if title == "INTERNET PROXY PROVIDER" {
                assert!(
                    rendered[height as usize - 1].contains("Esc/p close"),
                    "provider picker needs its own shortcuts at {width}x{height}"
                );
            }
            if matches!(title, "ACTIVE CONNECTIONS" | "NODE QUALITY") {
                assert!(
                    !rendered[height as usize - 1].contains("provider"),
                    "{title} leaks page shortcuts at {width}x{height}"
                );
                assert!(
                    rendered[0].contains(title),
                    "{title} title row at {width}x{height}"
                );
                assert!(
                    rendered[height as usize - 3].contains("Close"),
                    "{title} close control at {width}x{height}"
                );
                assert!(
                    rendered[height as usize - 2].starts_with('└'),
                    "{title} bottom border at {width}x{height}"
                );
                assert!(
                    rendered[height as usize - 2].ends_with('┘'),
                    "{title} bottom border at {width}x{height}"
                );
                if title == "NODE QUALITY" && (width, height) == (80, 24) {
                    assert!(
                        rendered[19].contains("untested"),
                        "compact evidence should show its value before scrolling"
                    );
                }
            }
        }
    }
}

#[test]
fn private_access_states_and_entry_dialogs_fit_the_viewport_matrix() {
    for &(width, height) in VIEWPORTS {
        let mut app = test_support::test_app();
        app.set_operational_workspace(OperationalWorkspace::PrivateAccess)
            .unwrap();

        for (state, expected) in [
            (PrivateAccessState::Disconnected, "DISCONNECTED"),
            (PrivateAccessState::Connecting, "CONNECTING"),
            (PrivateAccessState::Connected, "CONNECTED"),
            (PrivateAccessState::Disconnecting, "DISCONNECTING"),
            (PrivateAccessState::Error, "CONNECTION FAILED"),
        ] {
            app.private_access.focused_mut().state = state;
            app.private_access.focused_mut().last_error = Some("Gateway unavailable".into());
            let rendered = rows(&mut app, width, height);
            let text = rendered.join("\n");
            assert!(text.contains(expected), "{expected} at {width}x{height}");
            assert!(rendered[height as usize - 1].contains("GLOBAL NET"));
        }

        app.private_access.focused_mut().state = PrivateAccessState::Connected;
        for section in [IntranetDetailSection::Dns, IntranetDetailSection::Routes] {
            app.expanded_intranet_sections
                .insert(format!("hillstone:{}", section.key()));
            app.intranet_detail_section = section;
            let rendered = rows(&mut app, width, height);
            assert!(
                rendered.join("\n").contains("INTRANET:"),
                "expanded {section:?} at {width}x{height}"
            );
            assert!(rendered[height as usize - 1].contains("GLOBAL NET"));
        }

        app.private_access
            .profiles
            .push(PrivateAccessProfileRuntime::default_sonicwall().unwrap());
        app.open_provider_modal();
        let profile_picker_rows = rows(&mut app, width, height);
        let profile_picker = profile_picker_rows.join("\n");
        assert!(
            profile_picker.contains("INTRANET PROFILE"),
            "{width}x{height}"
        );
        assert!(profile_picker.contains("Esc"), "{width}x{height}");
        assert!(
            profile_picker_rows[height as usize - 1].contains("Esc/p close"),
            "profile picker needs its own shortcuts at {width}x{height}"
        );
        app.provider_modal = None;

        app.private_access_progress = Some(PrivateAccessProgressModal {
            profile_index: 0,
            title: "hillstone".into(),
            entries: vec![PrivateAccessProgressEntry {
                tone: PrivateAccessProgressTone::Info,
                text: "Waiting for gateway".into(),
            }],
            done: false,
        });
        let progress_rows = rows(&mut app, width, height);
        assert!(progress_rows.join("\n").contains("Waiting for gateway"));
        assert!(
            progress_rows[height as usize - 1].contains("Enter/Esc close"),
            "progress modal needs its own shortcuts at {width}x{height}"
        );
        app.private_access_progress = None;

        app.private_access_auth = Some(PrivateAccessAuthModal {
            profile_index: 0,
            service: "sonicwall".into(),
            session_id: "session".into(),
            challenge_id: "challenge".into(),
            title: "SONICWALL-HQ".into(),
            message: "Gateway requests a dynamic code".into(),
            fields: vec![PrivateAccessAuthField {
                id: "code".into(),
                label: "动态验证码".into(),
                kind: "text".into(),
                sensitive: false,
                required: true,
                options: vec![],
            }],
            buttons: vec![],
            inputs: vec![String::new()],
            field_index: 0,
            error: Some("Code is required".into()),
        });
        let auth_rows = rows(&mut app, width, height);
        let auth = auth_rows.join("\n");
        assert!(auth.contains("AUTHENTICATION"), "{width}x{height}");
        assert!(auth.contains("Code is required"), "{width}x{height}");
        assert!(auth.contains("Esc cancel"), "{width}x{height}");
        assert!(
            auth_rows[height as usize - 1].contains("Esc cancel"),
            "authentication modal needs its own shortcuts at {width}x{height}"
        );
        app.private_access_auth = None;

        app.onboarding = Some(OnboardingState {
            input: String::new(),
            message: String::new(),
        });
        let onboarding_rows = rows(&mut app, width, height);
        let onboarding = onboarding_rows.join("\n");
        assert!(onboarding.contains("First run setup"), "{width}x{height}");
        assert!(onboarding.contains("Esc"), "{width}x{height}");
        assert!(
            onboarding_rows[height as usize - 1].contains("s skip"),
            "first-run wizard needs its own shortcuts at {width}x{height}"
        );
    }
}

#[test]
fn cjk_route_and_candidate_do_not_displace_right_corner_status() {
    for &(width, height) in VIEWPORTS {
        let mut app = test_support::test_app();
        app.groups = vec![ProxyGroup {
            name: "宝贝云".into(),
            kind: "Selector".into(),
            current: Some("東京直连节点👩‍💻-very-long-candidate-name".into()),
            members: vec!["東京直连节点👩‍💻-very-long-candidate-name".into()],
        }];
        let rendered = rows(&mut app, width, height);
        assert!(rendered[0].contains("INTERNET"), "{width}x{height}");
        assert!(
            rendered[height as usize - 1].contains("GLOBAL NET"),
            "{width}x{height}"
        );
        assert!(
            rendered[height as usize - 1]
                .trim_end()
                .ends_with("↑0.0M/s")
        );
    }
}

#[test]
fn global_footer_uses_the_same_semantic_colors_in_every_workspace() {
    let theme = Theme::detect();
    let mut app = test_support::test_app();
    app.system_proxy =
        super::SystemProxy::for_test(app.system_proxy_config_path.clone(), "127.0.0.1:6780", true);
    for workspace in [
        OperationalWorkspace::Internet,
        OperationalWorkspace::PrivateAccess,
    ] {
        app.set_operational_workspace(workspace).unwrap();
        for view in [ActiveView::NodeList, ActiveView::NodeDashboard] {
            app.active_view = view;
            let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let buffer = terminal.backend().buffer();
            let label_x = (0..120)
                .find(|&x| buffer[(x, 29)].symbol() == "G")
                .expect("global label in footer");
            let down_x = (label_x..120)
                .find(|&x| buffer[(x, 29)].symbol() == "↓")
                .expect("download rate in footer");
            let status_x = (label_x..120)
                .find(|&x| buffer[(x, 29)].symbol() == "S")
                .expect("stable status in footer");
            assert_eq!(
                buffer[(label_x, 29)].fg,
                theme.text_muted(),
                "{workspace:?} {view:?}"
            );
            assert_eq!(
                buffer[(status_x, 29)].fg,
                theme.text_status_active(),
                "{workspace:?} {view:?}"
            );
            assert_eq!(
                buffer[(down_x, 29)].fg,
                theme.text_transfer(),
                "{workspace:?} {view:?}"
            );
        }
    }
}
