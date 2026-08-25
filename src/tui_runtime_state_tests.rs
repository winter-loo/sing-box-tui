use super::super::test_support::test_app;
use crate::controller::ProxyGroup;
use crate::private_access::PrivateAccessState;
use crate::tui_state::{PrivateAccessProfileState, TuiRuntimeState};

#[test]
fn apply_runtime_state_restores_explicit_china_ip_routing_intent() {
    let mut app = test_app();
    app.apply_runtime_state(TuiRuntimeState {
        china_ip_routing_enabled: Some(true),
        ..TuiRuntimeState::default()
    })
    .expect("state applies");

    assert!(app.china_ip_routing_enabled);
    assert!(app.china_ip_routing_explicit);
}

#[test]
fn stale_private_access_background_pid_is_discarded_from_state() {
    let mut app = test_app();
    let stale_pid = u32::MAX;
    assert!(!super::super::process_exists(stale_pid));
    let state = TuiRuntimeState {
        private_access_profiles: vec![PrivateAccessProfileState {
            id: "hillstone".to_string(),
            background_pid: Some(stale_pid),
            ..PrivateAccessProfileState::default()
        }],
        ..TuiRuntimeState::default()
    };

    app.apply_runtime_state(state).expect("state applies");

    assert_eq!(
        app.private_access.focused().state,
        PrivateAccessState::Disconnected
    );
    assert_eq!(app.private_access.focused().background_pid, None);
    assert_eq!(
        app.runtime_state().private_access_profiles[0].background_pid,
        None
    );
}

#[test]
fn unrelated_live_pid_is_discarded_from_private_access_state() {
    let mut app = test_app();
    let unrelated_pid = std::process::id();
    assert!(super::super::process_exists(unrelated_pid));
    let state = TuiRuntimeState {
        private_access_profiles: vec![PrivateAccessProfileState {
            id: "hillstone".to_string(),
            background_pid: Some(unrelated_pid),
            ..PrivateAccessProfileState::default()
        }],
        ..TuiRuntimeState::default()
    };

    app.apply_runtime_state(state).expect("state applies");

    assert_eq!(app.private_access.focused().background_pid, None);
    assert_eq!(
        app.private_access.focused().state,
        PrivateAccessState::Disconnected
    );
}

#[test]
fn private_access_uses_first_private_access_profile_as_initial_focus() {
    let mut app = test_app();
    let state = TuiRuntimeState {
        private_access_profiles: vec![
            PrivateAccessProfileState {
                id: "office-backup".to_string(),
                server: Some("sslvpn.backup.example.com".to_string()),
                username: Some("bob".to_string()),
                password: Some("backup-secret".to_string()),
                ..PrivateAccessProfileState::default()
            },
            PrivateAccessProfileState {
                id: "office".to_string(),
                server: Some("sslvpn.office.example.com".to_string()),
                username: Some("alice".to_string()),
                ..PrivateAccessProfileState::default()
            },
        ],
        ..TuiRuntimeState::default()
    };

    app.apply_runtime_state(state).expect("state applies");

    assert_eq!(app.private_access.focused_id(), "office-backup");
    assert_eq!(
        app.private_access.focused().server,
        "sslvpn.backup.example.com"
    );
    assert_eq!(app.private_access.profiles.len(), 2);
    let saved = app.runtime_state();
    assert_eq!(saved.private_access_profiles[0].id, "office-backup");
    assert_eq!(saved.private_access_profiles.len(), 2);
}

#[test]
fn app_applies_persisted_filter_auto_pick_and_selected_node() {
    let mut app = test_app();
    app.groups[0].members = vec![
        "node-a".to_string(),
        "node-b".to_string(),
        "node-c".to_string(),
    ];
    app.member_index = 0;
    let mut state = TuiRuntimeState {
        benchmark_filter: "node-b,node-c".to_string(),
        auto_pick_enabled: true,
        ..TuiRuntimeState::default()
    };
    state
        .current_selected_nodes
        .insert("select".to_string(), "node-c".to_string());

    app.apply_runtime_state(state).expect("state applies");

    assert_eq!(app.benchmark_filter, "node-b,node-c");
    assert!(app.auto_select_enabled);
    assert_eq!(app.member_index, 2);
}

#[test]
fn restore_plan_targets_changed_valid_selector_nodes() {
    let mut app = test_app();
    app.groups = vec![
        ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("node-a".to_string()),
            members: vec!["node-a".to_string(), "node-b".to_string()],
        },
        ProxyGroup {
            name: "auto".to_string(),
            kind: "URLTest".to_string(),
            current: Some("node-a".to_string()),
            members: vec!["node-a".to_string(), "node-b".to_string()],
        },
        ProxyGroup {
            name: "same".to_string(),
            kind: "Selector".to_string(),
            current: Some("node-a".to_string()),
            members: vec!["node-a".to_string(), "node-b".to_string()],
        },
        ProxyGroup {
            name: "stale".to_string(),
            kind: "Selector".to_string(),
            current: Some("node-a".to_string()),
            members: vec!["node-a".to_string()],
        },
    ];
    let mut state = TuiRuntimeState::default();
    state
        .current_selected_nodes
        .insert("select".to_string(), "node-b".to_string());
    state
        .current_selected_nodes
        .insert("auto".to_string(), "node-b".to_string());
    state
        .current_selected_nodes
        .insert("same".to_string(), "node-a".to_string());
    state
        .current_selected_nodes
        .insert("stale".to_string(), "node-missing".to_string());

    assert_eq!(
        app.persisted_selection_restore_plan(&state),
        vec![("select".to_string(), "node-b".to_string())]
    );
}

#[test]
fn app_applies_persisted_auto_pick_without_filter() {
    let mut app = test_app();
    let state = TuiRuntimeState {
        benchmark_filter: String::new(),
        auto_pick_enabled: true,
        ..TuiRuntimeState::default()
    };

    app.apply_runtime_state(state).expect("state applies");

    assert!(app.benchmark_filter.is_empty());
    assert!(app.auto_select_enabled);
}
