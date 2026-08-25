use super::super::test_support::test_app;
use crossterm::event::KeyCode;

use super::{
    SettingsEditState, SettingsField, settings_field_display_value, settings_field_value,
    visible_settings_fields,
};
use crate::private_access::PrivateAccessState;
use crate::private_access_session::{PrivateAccessMode, PrivateAccessProfileRuntime};
use crate::tui_state::{PrivateAccessProfileState, TuiRuntimeState};

#[test]
fn china_ip_routing_settings_field_reflects_enabled_state() {
    let mut app = test_app();
    assert_eq!(
        settings_field_value(&app, SettingsField::ChinaIpRouting),
        "false"
    );

    app.china_ip_routing_enabled = true;
    assert_eq!(
        settings_field_value(&app, SettingsField::ChinaIpRouting),
        "true"
    );
    assert!(visible_settings_fields(&app).contains(&SettingsField::ChinaIpRouting));
}

#[test]
fn private_access_mode_persists_and_can_switch_to_tun() {
    let mut app = test_app();

    app.apply_settings_value(SettingsField::PrivateAccessMode, "tun".to_string())
        .expect("mode applies");

    assert_eq!(
        settings_field_value(&app, SettingsField::PrivateAccessMode),
        "tun"
    );
    assert_eq!(
        app.runtime_state().private_access_profiles[0]
            .mode
            .as_deref(),
        Some("tun")
    );
}

#[test]
fn private_access_mode_change_while_connected_stays_in_settings() {
    let mut app = test_app();
    app.show_settings = true;
    app.private_access.focused_mut().state = PrivateAccessState::Connected;
    app.settings_edit = Some(SettingsEditState {
        field: SettingsField::PrivateAccessMode,
        input: "tun".to_string(),
        error: None,
    });

    assert!(
        app.handle_key(KeyCode::Enter)
            .expect("settings error is handled inside TUI")
    );

    assert!(app.show_settings);
    let error = app
        .settings_edit
        .as_ref()
        .and_then(|edit| edit.error.as_deref())
        .expect("settings error is shown inside settings panel");
    assert_eq!(app.private_access.focused().mode, PrivateAccessMode::Tun);
    assert!(error.contains("disconnect Private Access before changing data plane mode"));
}

#[test]
fn private_access_password_persists_and_settings_display_shows_it() {
    let mut app = test_app();
    app.private_access.focused_mut().password = "plain-secret".to_string();

    let state = app.runtime_state();
    assert_eq!(
        state.private_access_profiles[0].password.as_deref(),
        Some("plain-secret")
    );
    assert_eq!(
        settings_field_value(&app, SettingsField::PrivateAccessPassword),
        "plain-secret"
    );
    assert_eq!(
        settings_field_display_value(&app, SettingsField::PrivateAccessPassword),
        "plain-secret"
    );
}

#[test]
fn private_access_profile_setting_changes_focus_without_reordering_profiles() {
    let mut app = test_app();
    let state = TuiRuntimeState {
        private_access_profiles: vec![
            PrivateAccessProfileState {
                id: "office".to_string(),
                server: Some("sslvpn.office.example.com".to_string()),
                username: Some("alice".to_string()),
                ..PrivateAccessProfileState::default()
            },
            PrivateAccessProfileState {
                id: "backup-office".to_string(),
                server: Some("sslvpn.backup.example.com".to_string()),
                username: Some("bob".to_string()),
                ..PrivateAccessProfileState::default()
            },
        ],
        ..TuiRuntimeState::default()
    };
    app.apply_runtime_state(state).expect("state applies");

    app.apply_settings_value(
        SettingsField::PrivateAccessProfile,
        "backup-office".to_string(),
    )
    .expect("profile switches");

    assert_eq!(app.private_access.focused_id(), "backup-office");
    assert_eq!(app.runtime_state().private_access_profiles[0].id, "office");
}

#[test]
fn sonicwall_internet_proxy_setting_is_profile_scoped() {
    let mut app = test_app();
    assert!(!visible_settings_fields(&app).contains(&SettingsField::PrivateAccessUseInternetProxy));

    app.private_access
        .profiles
        .push(PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile"));
    app.private_access.focused_index = 1;
    assert!(visible_settings_fields(&app).contains(&SettingsField::PrivateAccessUseInternetProxy));

    app.apply_settings_value(
        SettingsField::PrivateAccessUseInternetProxy,
        "true".to_string(),
    )
    .expect("proxy choice saves");
    assert!(app.private_access.focused().use_internet_proxy);
    assert!(app.runtime_state().private_access_profiles[1].use_internet_proxy);
}
