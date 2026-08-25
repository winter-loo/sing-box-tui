use super::settings::{is_private_access_settings_field, visible_settings_fields};
use super::test_support::{
    test_app, test_app_without_private_access, test_bypass_rule_set_path, test_state_path,
};
use super::{PrivateAccessMode, PrivateAccessProfileRuntime, PrivateAccessState};
use crate::tui_state::{PrivateAccessProfileState, TuiRuntimeState, TuiStateStore};
use crossterm::event::KeyCode;

#[test]
fn private_access_domains_are_ephemeral_system_proxy_bypass_entries() {
    let mut app = test_app();
    app.bypass_entries = vec!["deeloo.cn".to_string()];
    let profile = app.private_access.focused_mut();
    profile.mode = PrivateAccessMode::Tun;
    profile.state = PrivateAccessState::Connecting;
    profile.domains = vec!["service.hundsun.com".to_string()];
    profile.domain_suffixes = vec!["Hundsun.COM".to_string(), "hs.handsome.com.cn".to_string()];
    let mut sonicwall =
        PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile");
    sonicwall.state = PrivateAccessState::Connected;
    sonicwall.domain_suffixes = vec!["hundsun.com".to_string()];
    app.private_access.profiles.push(sonicwall);

    assert_eq!(
        app.private_access
            .system_proxy_bypass_entries(&app.bypass_entries),
        vec![
            "deeloo.cn".to_string(),
            "service.hundsun.com".to_string(),
            "hundsun.com".to_string(),
            "hs.handsome.com.cn".to_string(),
        ]
    );
    assert_eq!(app.bypass_entries, vec!["deeloo.cn".to_string()]);

    app.private_access.focused_mut().state = PrivateAccessState::Disconnected;
    app.private_access.focused_mut().domains.clear();
    app.private_access.focused_mut().domain_suffixes.clear();
    assert_eq!(
        app.private_access
            .system_proxy_bypass_entries(&app.bypass_entries),
        vec!["deeloo.cn".to_string(), "hundsun.com".to_string()]
    );

    app.private_access.profiles[1].state = PrivateAccessState::Disconnected;
    app.private_access.profiles[1].domains.clear();
    app.private_access.profiles[1].domain_suffixes.clear();
    assert_eq!(
        app.private_access
            .system_proxy_bypass_entries(&app.bypass_entries),
        vec!["deeloo.cn".to_string()]
    );
}

#[test]
fn private_access_is_absent_without_configured_profiles() {
    let app = test_app_without_private_access();

    assert!(!app.private_access.is_configured());
    assert!(app.runtime_state().private_access_profiles.is_empty());
    assert!(
        visible_settings_fields(&app)
            .iter()
            .all(|field| !is_private_access_settings_field(*field))
    );
}

#[test]
fn private_access_background_session_is_shown_as_background() {
    let mut app = test_app();
    let pid = std::process::id();
    let focused = app.private_access.focused_mut();
    focused.server = "sslvpn.example.com".to_string();
    focused.username = "alice".to_string();
    focused.background_pid = Some(pid);
    focused.state = PrivateAccessState::Connected;

    assert_eq!(
        app.private_access.focused().state,
        PrivateAccessState::Connected
    );
    assert_eq!(app.private_access.focused().background_pid, Some(pid));
    assert_eq!(
        app.runtime_state().private_access_profiles[0].background_pid,
        Some(pid)
    );
}

#[test]
fn private_access_tun_helper_persists_from_json_state() {
    let mut app = test_app();
    let state = TuiRuntimeState {
        private_access_profiles: vec![PrivateAccessProfileState {
            id: "office-tun".to_string(),
            mode: Some("tun".to_string()),
            server: Some("sslvpn.example.com".to_string()),
            username: Some("alice".to_string()),
            tun_helper: Some(vec![
                "sudo".to_string(),
                "-n".to_string(),
                "/opt/sing-box-tui".to_string(),
                "private-access-tun-helper".to_string(),
                "--stdio".to_string(),
            ]),
            ..PrivateAccessProfileState::default()
        }],
        ..TuiRuntimeState::default()
    };

    app.apply_runtime_state(state).expect("state applies");
    let saved = app.runtime_state();

    assert_eq!(
        saved.private_access_profiles[0]
            .tun_helper
            .as_ref()
            .unwrap(),
        &[
            "sudo",
            "-n",
            "/opt/sing-box-tui",
            "private-access-tun-helper",
            "--stdio"
        ]
    );
}

#[test]
fn tui_state_store_round_trips_filter_auto_pick_and_current_nodes() {
    let path = test_state_path();
    let store = TuiStateStore::new(&path);
    let mut state = TuiRuntimeState {
        benchmark_filter: "美国,香港".to_string(),
        auto_pick_enabled: true,
        bypass_entries: vec!["example.com".to_string(), "10.0.0.0/8".to_string()],
        ..TuiRuntimeState::default()
    };
    state
        .current_selected_nodes
        .insert("select".to_string(), "node-a".to_string());

    store.save(&state).expect("save state");
    let loaded = store.load().expect("load state");

    assert_eq!(loaded, state);
    let _ = std::fs::remove_file(path);
}

#[test]
fn bypass_rule_set_store_writes_domains_and_ip_cidrs() {
    let path = test_bypass_rule_set_path();
    let store = crate::tui_state::BypassRuleSetStore::new(&path);

    store
        .save(&[
            "example.com".to_string(),
            "*.github.com".to_string(),
            "1.1.1.1".to_string(),
            "10.0.0.0/8".to_string(),
        ])
        .expect("save rule set");

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read rule set"))
            .expect("parse rule set");
    assert_eq!(value["version"], 1);
    assert_eq!(
        value["rules"][0]["domain_suffix"],
        serde_json::json!(["example.com", "github.com"])
    );
    assert_eq!(
        value["rules"][1]["ip_cidr"],
        serde_json::json!(["1.1.1.1", "10.0.0.0/8"])
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn filter_and_auto_pick_changes_are_saved_to_tui_state() {
    let path = test_state_path();
    let mut app = test_app();
    app.state_store = Some(TuiStateStore::new(&path));

    app.apply_benchmark_filter("香港".to_string())
        .expect("apply filter");
    app.handle_key(KeyCode::Char('a'))
        .expect("toggle auto-pick");

    let state = TuiStateStore::new(&path).load().expect("load state");
    assert_eq!(state.benchmark_filter, "香港");
    assert!(state.auto_pick_enabled);
    assert_eq!(state.auto_pick_selector.as_deref(), Some("select"));
    assert_eq!(
        state.current_selected_nodes.get("select"),
        Some(&"node-a".to_string())
    );

    let _ = std::fs::remove_file(path);
}
