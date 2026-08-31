use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use serde_json::json;

use super::super::test_support::{test_app, test_state_path};
use crate::internet_tun::{InternetTunTransaction, PersistedInternetTun};

#[test]
fn backslash_starts_the_internet_tun_transition() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sing-box-tui-tun-toggle-{nanos}.json"));
    std::fs::write(
        &path,
        r#"{"inbounds":[{"type":"mixed","listen":"::","listen_port":6780,"set_system_proxy":false}],"outbounds":[{"type":"direct","tag":"direct"}]}"#,
    )
    .expect("write temp config");

    let mut app = test_app();
    app.system_proxy_config_path = path.clone();
    app.internet_tun = InternetTunTransaction::new(path.clone(), PersistedInternetTun::default())
        .expect("Internet TUN transaction initializes");
    app.handle_key(KeyCode::Char('\\'))
        .expect("backslash is handled");

    assert!(app.internet_tun.is_transitioning());
    assert_eq!(app.status, "Enabling TUN mode...");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::fs::read_to_string(&path).is_ok_and(|text| text.contains("\"tun\"")) {
        assert!(Instant::now() < deadline, "config mutation timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn reserved_tun_tag_conflict_does_not_prompt_for_sudo() {
    let config_path = test_state_path();
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&json!({
            "inbounds": [{
                "type": "mixed",
                "tag": "tun-in",
                "listen": "::",
                "listen_port": 6780
            }]
        }))
        .expect("config serializes"),
    )
    .expect("config writes");
    let mut app = test_app();
    app.system_proxy_config_path = config_path.clone();
    app.internet_tun =
        InternetTunTransaction::new(config_path.clone(), PersistedInternetTun::default())
            .expect("Internet TUN transaction initializes");

    assert!(!app.tun_toggle_needs_terminal_prompt());

    let _ = std::fs::remove_file(config_path);
}
