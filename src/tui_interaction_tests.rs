use super::test_support::test_app;
use super::{
    LeftPaneSection, private_access_auth_display_value, private_access_auth_initial_value,
    truncate_for_width,
};
use crate::private_access::PrivateAccessAuthField;
use crate::private_access_session::PrivateAccessRuntime;
use crate::tui_state::{PrivateAccessProfileState, TuiRuntimeState};
use crossterm::event::KeyCode;

#[test]
fn tun_toggle_is_documented_in_help_and_status_bar() {
    let mut app = test_app();
    let snapshot = app.view_snapshot();
    assert!(!snapshot.status.system_proxy_enabled);
    assert!(!snapshot.status.tun_enabled);
    assert!(!snapshot.status.selection_context.contains("Controller:"));
    assert!(!snapshot.status.selection_context.contains("order="));
    assert!(!snapshot.status.selection_context.contains("Arrows/jk"));
}

#[test]
fn status_header_tracks_filter_clash_and_pick_mode() {
    let mut app = test_app();
    app.auto_select_enabled = false;
    app.benchmark_filter = "-香港,-广告".to_string();

    let snapshot = app.view_snapshot();
    let context = &snapshot.status.selection_context;
    assert!(context.contains("filter='-香港,-广告'"));
    assert!(context.contains("clash="));
    assert!(context.contains("Pick=Manual"));
    assert!(!context.contains("tested="));
    assert!(!context.contains("auto="));
    app.auto_select_enabled = true;

    let snapshot = app.view_snapshot();
    let context = &snapshot.status.selection_context;
    assert!(context.contains("Pick=Auto"));
}

#[test]
fn status_header_keeps_empty_filter_label_in_intranet_details() {
    let mut app = test_app();
    app.benchmark_filter.clear();
    app.left_pane_section = LeftPaneSection::Intranet;

    let snapshot = app.view_snapshot();
    assert!(snapshot.status.selection_context.contains("filter=''"));
    assert!(
        snapshot
            .status
            .selection_context
            .contains("Intranet details are shown in the right panel")
    );
}

#[test]
fn truncates_wide_strings_without_panicking() {
    let truncated = truncate_for_width("手动选择-自动选择-节点A", 8);
    assert!(truncated.ends_with('…'));
    assert!(!truncated.is_empty());
}

#[test]
fn question_mark_opens_and_closes_help() {
    let mut app = test_app();

    app.handle_key(KeyCode::Char('?')).expect("open help");

    assert!(app.show_help);
    assert_eq!(app.status, "Showing help");

    app.handle_key(KeyCode::Esc).expect("close help");

    assert!(!app.show_help);
    assert_eq!(app.status, "Help closed");
}

#[test]
fn help_panel_moves_selection_with_keyboard() {
    let mut app = test_app();
    app.handle_key(KeyCode::Char('?')).expect("open help");

    app.handle_key(KeyCode::Down).expect("move down");
    assert_eq!(app.help_index, 1);

    app.handle_key(KeyCode::Char('j')).expect("move down");
    assert_eq!(app.help_index, 2);

    app.handle_key(KeyCode::Up).expect("move up");
    assert_eq!(app.help_index, 1);

    app.handle_key(KeyCode::Char('k')).expect("move up");
    assert_eq!(app.help_index, 0);
}

#[test]
fn status_only_updates_clear_flash() {
    let mut app = test_app();

    app.set_status_with_flash("flash me");
    assert!(app.flash.is_some());

    app.set_status_only("status only");

    assert_eq!(app.status, "status only");
    assert!(app.flash.is_none());
}

#[test]
fn switching_selection_updates_status_without_flash_popup() {
    let mut app = test_app();
    app.set_status_with_flash("old flash");
    app.set_switch_status("select", "node-b");

    assert_eq!(app.status, "Switched select to node-b");
    assert!(app.flash.is_none());
}

#[test]
fn sonicwall_auth_displays_secrets_and_prefills_only_static_credentials() {
    let secret_field = PrivateAccessAuthField {
        id: "reply-2".to_string(),
        label: "Dynamic code".to_string(),
        kind: "password".to_string(),
        sensitive: true,
        required: true,
        options: Vec::new(),
    };
    assert_eq!(
        private_access_auth_display_value(&secret_field, "123456"),
        "123456"
    );

    let persisted = TuiRuntimeState {
        private_access_profiles: vec![PrivateAccessProfileState {
            id: "sonicwall".to_string(),
            server: Some("sslvpn.example.com".to_string()),
            username: Some("alice".to_string()),
            password: Some("static-secret".to_string()),
            password_env: Some("SONICWALL_PASSWORD".to_string()),
            ..PrivateAccessProfileState::default()
        }],
        ..TuiRuntimeState::default()
    };
    let mut runtime = PrivateAccessRuntime::new().expect("runtime builds");
    runtime
        .apply_state(&persisted, |_, _| false)
        .expect("SonicWall profile loads");
    let profile = runtime.focused();

    let username_field = PrivateAccessAuthField {
        id: "reply-0".to_string(),
        label: "Domain account".to_string(),
        kind: "text is-username".to_string(),
        sensitive: false,
        required: true,
        options: Vec::new(),
    };
    let password_field = PrivateAccessAuthField {
        id: "reply-1".to_string(),
        label: "Domain password".to_string(),
        kind: "password is-password".to_string(),
        sensitive: true,
        required: true,
        options: Vec::new(),
    };
    assert_eq!(
        private_access_auth_initial_value(profile, &username_field),
        "alice"
    );
    assert_eq!(
        private_access_auth_initial_value(profile, &password_field),
        "static-secret"
    );
    assert_eq!(
        private_access_auth_initial_value(profile, &secret_field),
        ""
    );

    let state = runtime.runtime_states(|_| false).remove(0);
    assert_eq!(state.username.as_deref(), Some("alice"));
    assert_eq!(state.password.as_deref(), Some("static-secret"));
    assert_eq!(state.password_env.as_deref(), Some("SONICWALL_PASSWORD"));
}
