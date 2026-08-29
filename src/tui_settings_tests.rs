use super::super::test_support::test_app;
use crossterm::event::KeyCode;

use super::{
    SettingsEditState, SettingsField, settings_field_display_value, settings_field_value,
    visible_settings_fields,
};
use crate::private_access::PrivateAccessState;
use crate::private_access_session::{PrivateAccessMode, PrivateAccessProfileRuntime};
use crate::storage::BenchmarkStore;
use crate::sustained_quality::{
    NodeSustainedQuality, SustainedCompletion, SustainedProbeOutcome, sustained_target_identity,
};
use crate::tui_state::{PrivateAccessProfileState, TuiRuntimeState};
use crate::{controller::NodeReachabilityAssessment, controller::ProbeOutcome};

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

#[test]
fn sustained_target_setting_requires_account_free_https() {
    let mut app = test_app();
    assert!(
        app.apply_settings_value(
            SettingsField::SustainedTargetUrl,
            "http://example.test/payload".to_string(),
        )
        .is_err()
    );
    assert!(
        app.apply_settings_value(
            SettingsField::SustainedTargetUrl,
            "https://example.test/payload?token=secret".to_string(),
        )
        .is_err()
    );

    app.apply_settings_value(
        SettingsField::SustainedTargetUrl,
        "https://example.test/payload?bytes=524288".to_string(),
    )
    .unwrap();
    assert_eq!(
        app.runtime_state().sustained_target_url.as_deref(),
        Some("https://example.test/payload?bytes=524288")
    );
}

#[test]
fn changing_sustained_target_resynchronizes_streaming_selection() {
    let mut app = test_app();
    let target_a = "https://a.example.test/payload?bytes=524288";
    let target_b = "https://b.example.test/payload?bytes=524288";
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sing-box-tui-target-selection-{nanos}.sqlite3"));
    let store = BenchmarkStore::open(&path).unwrap();
    store
        .reconcile_node_history(&serde_json::json!({
            "outbounds": [
                {"type":"direct", "tag":"node-a"},
                {"type":"direct", "tag":"node-b"}
            ]
        }))
        .unwrap();
    for (target, node, completion_ms) in [(target_a, "node-a", 600), (target_b, "node-b", 400)] {
        store
            .record_sustained_quality(
                "select",
                &sustained_target_identity(target).unwrap(),
                &NodeSustainedQuality {
                    name: node.into(),
                    outcome: SustainedProbeOutcome::Completed(SustainedCompletion {
                        first_byte_ms: 100,
                        completion_ms,
                        bytes_read: 512 * 1024,
                        throughput_bytes_per_second: 1,
                    }),
                },
            )
            .unwrap();
    }
    app.benchmark_workflow.replace_store(Some(store));
    app.benchmark_workflow
        .activate_sustained_target(target_a)
        .unwrap();
    app.sustained_target_url = target_a.into();
    app.groups[0].members = vec!["node-a".into(), "node-b".into()];
    for node in ["node-a", "node-b"] {
        app.benchmark_workflow.set_reachability_assessment(
            "select",
            NodeReachabilityAssessment::from_attempts(
                node.into(),
                vec![
                    ProbeOutcome::Reachable { delay_ms: 20 },
                    ProbeOutcome::Reachable { delay_ms: 30 },
                    ProbeOutcome::Reachable { delay_ms: 40 },
                ],
            ),
        );
    }
    app.move_node_view_next();
    assert_eq!(app.selected_member_name().as_deref(), Some("node-a"));

    app.apply_settings_value(SettingsField::SustainedTargetUrl, target_b.into())
        .unwrap();

    assert_eq!(app.displayed_members(), ["node-b"]);
    assert_eq!(app.selected_member_name().as_deref(), Some("node-b"));
    drop(app);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}
