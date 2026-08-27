use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Client as AsyncClient;
use tokio::runtime::Builder as TokioRuntimeBuilder;

use super::{
    App, BenchmarkWorkflow, CONNECTION_REFRESH_INTERVAL, DIRECT_CLASH_MODE, Focus,
    GLOBAL_CLASH_MODE, LeftPaneSection, PrivateAccessRuntime, RULE_CLASH_MODE, SystemProxy,
};
use crate::controller::{ApiClient, ConnectionsSnapshot, ProxyGroup};
use crate::defaults::{DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONTROLLER};
use crate::internet_tun::{InternetTunTransaction, PersistedInternetTun};
use crate::managed_sing_box::ManagedSingBox;

static TEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_test_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let counter = TEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{counter}")
}

pub(super) fn test_db_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "sing-box-tui-tui-test-{}.sqlite3",
        unique_test_suffix()
    ))
}

pub(super) fn test_state_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "sing-box-tui-state-test-{}.json",
        unique_test_suffix()
    ))
}

pub(super) fn test_bypass_rule_set_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "sing-box-tui-bypass-test-{}.json",
        unique_test_suffix()
    ))
}

pub(super) fn test_app() -> App {
    let runtime = TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let client = AsyncClient::builder()
        .no_proxy()
        .build()
        .expect("test HTTP client");
    let api_client = ApiClient {
        base_url: DEFAULT_CONTROLLER.to_string(),
        runtime,
        client,
    };
    let benchmark_workflow =
        BenchmarkWorkflow::for_test(api_client.base_url.clone(), api_client.client.clone());
    let config_path = std::env::temp_dir().join(format!(
        "sing-box-tui-config-test-{}.json",
        unique_test_suffix()
    ));

    App {
        client: api_client,
        groups: vec![ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("node-a".to_string()),
            members: vec!["node-a".to_string()],
        }],
        group_index: 0,
        internet_route_index: 0,
        member_index: 0,
        focus: Focus::Members,
        left_pane_section: LeftPaneSection::Internet,
        intranet_detail_scroll: 0,
        expanded_intranet_sections: BTreeSet::new(),
        status: String::new(),
        flash: None,
        benchmark_filter: "美国".to_string(),
        benchmark_url: "https://www.gstatic.com/generate_204".to_string(),
        benchmark_timeout_ms: 5000,
        benchmark_request_timeout: 12.0,
        benchmark_max_concurrency: DEFAULT_BENCHMARK_MAX_CONCURRENCY,
        verify_targets: super::default_verification_targets_setting(),
        benchmark_workflow,
        filter_input: None,
        bypass_input: None,
        bypass_entries: Vec::new(),
        auto_select_enabled: false,
        auto_select_selector: None,
        auto_select_threshold_ms: 600,
        auto_select_interval: Duration::from_secs(30),
        last_auto_select_benchmark: None,
        background_started_at_unix: super::current_unix_timestamp(),
        background_auto_pick: Default::default(),
        state_store: None,
        bypass_rule_set_store: None,
        latency_chart: None,
        clash_mode: Some(RULE_CLASH_MODE.to_string()),
        clash_modes: vec![
            GLOBAL_CLASH_MODE.to_string(),
            DIRECT_CLASH_MODE.to_string(),
            RULE_CLASH_MODE.to_string(),
        ],
        connections: ConnectionsSnapshot::default(),
        connection_error: None,
        last_connection_refresh: Instant::now() - CONNECTION_REFRESH_INTERVAL,
        show_connections: false,
        show_help: false,
        help_index: 0,
        onboarding_complete: true,
        onboarding: None,
        show_settings: false,
        settings_index: 0,
        settings_edit: None,
        settings_error: None,
        subscription_refresh: None,
        system_proxy_config_path: config_path.clone(),
        system_proxy: SystemProxy::for_test(config_path.clone(), "127.0.0.1:6780", false),
        internet_tun: InternetTunTransaction::new(
            config_path.clone(),
            PersistedInternetTun::default(),
        )
        .expect("Internet TUN transaction initializes"),
        china_ip_routing_enabled: false,
        china_ip_routing_explicit: false,
        tailscale_enabled: false,
        tailscale_explicit: false,
        tailscale_tailnet_domain: String::new(),
        tailscale_hostname: String::new(),
        verify_job: None,
        sing_box: ManagedSingBox::new(PathBuf::from("sing-box"), config_path, false),
        private_access: PrivateAccessRuntime::with_default_hillstone()
            .expect("private access runtime"),
        private_access_progress: None,
        private_access_auth: None,
    }
}

pub(super) fn private_access_progress_text(app: &App) -> String {
    app.private_access_progress
        .as_ref()
        .map(|progress| {
            progress
                .entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

pub(super) fn internet_routes_app() -> App {
    let mut app = test_app();
    app.groups = vec![
        ProxyGroup {
            name: "手动选择".to_string(),
            kind: "Selector".to_string(),
            current: Some("宝贝云".to_string()),
            members: vec![
                "自动选择".to_string(),
                "AirTCP".to_string(),
                "宝贝云".to_string(),
            ],
        },
        ProxyGroup {
            name: "自动选择".to_string(),
            kind: "URLTest".to_string(),
            current: Some("auto-node".to_string()),
            members: vec![
                "auto-node".to_string(),
                "air-1".to_string(),
                "bby-1".to_string(),
            ],
        },
        ProxyGroup {
            name: "AirTCP".to_string(),
            kind: "Selector".to_string(),
            current: Some("air-1".to_string()),
            members: vec!["air-1".to_string(), "air-2".to_string()],
        },
        ProxyGroup {
            name: "宝贝云".to_string(),
            kind: "Selector".to_string(),
            current: Some("bby-2".to_string()),
            members: vec!["bby-1".to_string(), "bby-2".to_string()],
        },
    ];
    app.group_index = 0;
    app.internet_route_index = 1;
    app.member_index = 1;
    app.benchmark_filter.clear();
    app
}

pub(super) fn test_app_without_private_access() -> App {
    let mut app = test_app();
    app.private_access = PrivateAccessRuntime::new().expect("empty private access runtime");
    app
}
