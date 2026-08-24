use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io;
use std::net::SocketAddrV4;
use std::path::PathBuf;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;

use crate::auto_pick::{
    AutoPickConfig, AutoPickDecision, BACKGROUND_TASK_KIND, BackgroundAutoPickManager,
    BackgroundLatencyResult, BackgroundLatencySnapshot, BackgroundLaunchSpec, BackgroundPollEvent,
    BackgroundStatusSnapshot, BackgroundWorkerEnsure, HeadlessWorkerCommand, HeadlessWorkerControl,
    HeadlessWorkerMetadata, registered_status_value, stop_registered_worker,
};
use crate::config::{
    PrivateAccessRouteTableOptions, china_ip_routing_ruleset_dir, config_has_china_ip_routing,
    run_private_access_route_table_config, run_private_access_tun_baseline_config,
    set_china_ip_routing,
};
use crate::controller::{
    ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest, BenchmarkResult,
    BenchmarkSummary, ConnectionInfo, ConnectionsSnapshot, ProxyGroup, VerificationReport,
    VerificationTarget, matches_filter, run_verification, spawn_benchmark_worker,
};
use crate::defaults::{
    DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONTROLLER, DEFAULT_DELAY_TEST_URL,
    DEFAULT_SELECTOR_TAG, DEFAULT_VERIFICATION_TARGETS, REFRESH_DEBOUNCE,
    SINGLE_NODE_RETEST_DEBOUNCE,
};
use crate::internet_tun::{
    InternetTunTarget, InternetTunToggleOutcome, InternetTunTransaction, PersistedInternetTun,
};
use crate::managed_sing_box::{
    AuthorizationRequirement, ControllerProbe, ManagedSingBox, RestartReceipt,
    wait_for_controller_ready,
};
use crate::private_access::{
    PrivateAccessSecret, PrivateAccessServiceManifest, PrivateAccessState,
};
use crate::private_access_session::{
    PrivateAccessBridgeRouteUpdate, PrivateAccessCarrierRestart, PrivateAccessConnectOptions,
    PrivateAccessDisconnectOutcome, PrivateAccessMode, PrivateAccessNetworkIntegration,
    PrivateAccessNoticeTone, PrivateAccessProfileRuntime, PrivateAccessRuntime,
    PrivateAccessSessionNotice, load_manifest_for_profile, parse_private_access_mode,
};
use crate::ruleset::download_china_ip_routing_rulesets;
use crate::storage::{BenchmarkRecord, BenchmarkStore, default_benchmark_db_path};
use crate::subscriptions::{
    DEFAULT_SUBSCRIPTION_SOURCE_PATH, SubscriptionRefreshOutput, SubscriptionRefreshRequest,
    refresh_subscriptions,
};
use crate::system_proxy::{SystemProxy, SystemProxyToggle, SystemProxyUpdate};
use crate::tui_state::{
    BypassRuleSetStore, TuiRuntimeState, TuiStateStore, default_bypass_rule_set_path,
    default_tui_state_path, parse_bypass_entries,
};

#[path = "tui_presentation.rs"]
mod presentation;
#[path = "tui_private_access.rs"]
mod private_access_workflow;
#[cfg(test)]
use presentation::private_access_auth_display_value;
use presentation::{
    CandidateRow, CandidateTone, ConnectionsPanelSnapshot, DashboardSnapshot, Focus, InternetRow,
    IntranetDetailSection, IntranetDetailSnapshot, IntranetDetailView, IntranetRow,
    LatencyChartState, LeftPaneSection, OnboardingState, PrivateAccessAuthModal,
    PrivateAccessProgressEntry, PrivateAccessProgressModal, PrivateAccessProgressTone,
    SETTINGS_FIELDS, SettingRow, SettingsEditState, SettingsField, SettingsPanelSnapshot,
    StatusFooter, StatusSnapshot, format_bytes_opt, format_duration_badge, help_binding_count,
    latency_chart_window_label, latency_chart_zoom_in, latency_chart_zoom_out, node_order_badge,
    pick_mode_badge, private_access_auth_initial_value, private_access_detail_view,
    private_access_progress_title, settings_field_label, subscription_report_badge,
    truncate_for_width,
};
use private_access_workflow::private_access_process_exists;

const AUTO_SELECT_INTERVAL: Duration = Duration::from_secs(30);
const AUTO_SELECT_THRESHOLD_MS: u64 = 600;
const CONNECTION_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LATENCY_CHART_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SUBSCRIPTION_REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const LATENCY_CHART_DEFAULT_WINDOW: Duration = Duration::from_secs(60 * 60);
const DIRECT_CLASH_MODE: &str = "直连";
const RULE_CLASH_MODE: &str = "规则";
const GLOBAL_CLASH_MODE: &str = "全局";

impl ControllerProbe for ApiClient {
    fn probe_controller(&self) -> Result<()> {
        self.fetch_config().map(|_| ())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TuiSubscriptionRefreshOptions {
    pub(crate) input: PathBuf,
    pub(crate) cache_path: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) disabled: bool,
    pub(crate) force: bool,
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
    pub(crate) interval_days: u64,
}

pub(crate) fn run_tui(
    controller: Option<String>,
    max_concurrency: Option<usize>,
    sing_box_executable: PathBuf,
    keep_sing_box_running: bool,
    subscription_refresh: TuiSubscriptionRefreshOptions,
) -> Result<()> {
    let controller = controller
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());

    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());

    let mut app = App::new(
        ApiClient::new(controller, secret)?,
        max_concurrency.unwrap_or(DEFAULT_BENCHMARK_MAX_CONCURRENCY),
        subscription_refresh,
        sing_box_executable,
        keep_sing_box_running,
        true,
    )?;
    app.ensure_auto_pick_background_worker_if_enabled()?;
    let terminal = setup_terminal()?;
    let result = run_app(terminal, &mut app);
    let restore_result = restore_terminal();
    let shutdown_result = app.shutdown_managed_sing_box();
    result.and(restore_result).and(shutdown_result)
}

pub(crate) fn run_headless_auto_pick(
    controller: Option<String>,
    max_concurrency: Option<usize>,
    subscription_refresh: TuiSubscriptionRefreshOptions,
) -> Result<()> {
    let controller = controller
        .or_else(|| env::var("SING_BOX_CONTROLLER").ok())
        .unwrap_or_else(|| DEFAULT_CONTROLLER.to_string());
    let secret = env::var("SING_BOX_SECRET")
        .ok()
        .filter(|value| !value.is_empty());
    let mut app = App::new(
        ApiClient::new(controller, secret)?,
        max_concurrency.unwrap_or(DEFAULT_BENCHMARK_MAX_CONCURRENCY),
        TuiSubscriptionRefreshOptions {
            disabled: true,
            ..subscription_refresh
        },
        PathBuf::from("sing-box"),
        true,
        false,
    )?;
    app.run_headless_auto_pick_loop()
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) fn run_background_status() -> Result<()> {
    print_json(registered_status_value()?)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub(crate) fn run_background_status() -> Result<()> {
    bail!("background process status is only available on Windows, macOS, and Linux")
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) fn run_background_stop() -> Result<()> {
    let Some(pid) = stop_registered_worker()? else {
        disable_persisted_auto_pick()?;
        print_json(serde_json::json!({ "status": "none" }))?;
        return Ok(());
    };
    disable_persisted_auto_pick()?;
    print_json(serde_json::json!({
        "status": "stopped",
        "kind": BACKGROUND_TASK_KIND,
        "pid": pid,
        "was_running": true,
    }))?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub(crate) fn run_background_stop() -> Result<()> {
    bail!("background process stop is only available on Windows, macOS, and Linux")
}

fn disable_persisted_auto_pick() -> Result<()> {
    let store = TuiStateStore::new(default_tui_state_path());
    if !store.exists() {
        return Ok(());
    }
    let mut state = store.load()?;
    state.auto_pick_enabled = false;
    state.auto_pick_selector = None;
    store.save(&state)
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn print_json(value: Value) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(&value).context("failed to encode JSON output")?
    );
    Ok(())
}

fn setup_terminal() -> Result<DefaultTerminal> {
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    Ok(ratatui::DefaultTerminal::new(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
    )?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    Ok(())
}

fn run_app(mut terminal: DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.poll_benchmark_updates()?;
        app.poll_subscription_refresh_updates()?;
        app.poll_system_proxy_updates();
        app.poll_tun_toggle_updates();
        app.poll_private_access_updates()?;
        app.poll_verify_updates();
        app.poll_background_auto_pick_status()?;
        app.maybe_start_subscription_refresh();
        app.maybe_refresh_latency_chart()?;
        app.maybe_refresh_connections();
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if matches!(key.code, KeyCode::Char('V'))
                    && app.private_access_connect_needs_terminal_prompt()
                {
                    app.connect_private_access_with_terminal_prompt(&mut terminal)?;
                } else if matches!(key.code, KeyCode::Char('\\'))
                    && app.tun_toggle_needs_terminal_prompt()
                {
                    toggle_tun_with_terminal_prompt(&mut terminal, app)?;
                } else if !app.handle_key(key.code)? {
                    return Ok(());
                }
            }
            Event::Mouse(mouse) => app.handle_mouse(mouse.kind),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn suspend_terminal_for_prompt(terminal: &mut DefaultTerminal, message: &str) -> Result<()> {
    terminal.show_cursor()?;
    restore_terminal()?;
    println!();
    println!("{message}");
    println!("Complete the sudo prompt below; the TUI will resume immediately afterward.");
    println!();
    Ok(())
}

fn resume_terminal_after_prompt(terminal: &mut DefaultTerminal) -> Result<()> {
    enable_raw_mode().context("failed to re-enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
        .context("failed to re-enter alternate screen")?;
    terminal.clear()?;
    Ok(())
}

fn toggle_tun_with_terminal_prompt(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    let action = if app.internet_tun.is_enabled() {
        "Disabling"
    } else {
        "Enabling"
    };
    app.set_status_only(format!(
        "{action} TUN mode needs administrator authorization..."
    ));
    terminal.draw(|frame| draw(frame, app))?;
    suspend_terminal_for_prompt(
        terminal,
        "TUN mode needs administrator authorization to update the network interface.",
    )?;
    let authorization = Command::new("sudo")
        .arg("-v")
        .status()
        .context("failed to start sudo authorization for TUN mode");
    let resume_result = resume_terminal_after_prompt(terminal);
    resume_result?;
    let status = authorization?;
    if !status.success() {
        app.set_status_with_flash(format!("TUN mode sudo authorization failed: {status}"));
        return Ok(());
    }
    app.toggle_tun_mode();
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let snapshot = app.presentation_snapshot();
    presentation::render(frame, &snapshot);
}

fn visible_settings_fields(app: &App) -> Vec<SettingsField> {
    SETTINGS_FIELDS
        .iter()
        .copied()
        .filter(|field| {
            !is_private_access_settings_field(*field) || app.private_access.is_configured()
        })
        .filter(|field| {
            *field != SettingsField::PrivateAccessUseInternetProxy
                || app
                    .private_access
                    .focused_opt()
                    .is_some_and(|profile| profile.manifest.id == "sonicwall")
        })
        .collect()
}

fn is_private_access_settings_field(field: SettingsField) -> bool {
    matches!(
        field,
        SettingsField::PrivateAccessProfile
            | SettingsField::PrivateAccessManifestPath
            | SettingsField::PrivateAccessMode
            | SettingsField::PrivateAccessServer
            | SettingsField::PrivateAccessPort
            | SettingsField::PrivateAccessUsername
            | SettingsField::PrivateAccessPassword
            | SettingsField::PrivateAccessPasswordEnv
            | SettingsField::PrivateAccessBridgeListen
            | SettingsField::PrivateAccessUseInternetProxy
            | SettingsField::PrivateAccessTlsVerify
    )
}

fn settings_field_value(app: &App, field: SettingsField) -> String {
    match field {
        SettingsField::BenchmarkUrl => app.benchmark_url.clone(),
        SettingsField::BenchmarkTimeoutMs => app.benchmark_timeout_ms.to_string(),
        SettingsField::RequestTimeoutSec => app.benchmark_request_timeout.to_string(),
        SettingsField::MaxConcurrency => app.benchmark_max_concurrency.to_string(),
        SettingsField::VerifyTargets => app.verify_targets.clone(),
        SettingsField::AutoPickThresholdMs => app.auto_select_threshold_ms.to_string(),
        SettingsField::AutoPickIntervalSec => app.auto_select_interval.as_secs().to_string(),
        SettingsField::SystemProxyServer => app.system_proxy.server().to_string(),
        SettingsField::ChinaIpRouting => app.china_ip_routing_enabled.to_string(),
        SettingsField::PrivateAccessProfile => app
            .private_access
            .focused_opt()
            .map(|profile| profile.id.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessManifestPath => app
            .private_access
            .focused_opt()
            .map(|profile| profile.manifest_path.clone().unwrap_or_default())
            .unwrap_or_default(),
        SettingsField::PrivateAccessMode => app
            .private_access
            .focused_opt()
            .map(|profile| profile.mode.as_str().to_string())
            .unwrap_or_default(),
        SettingsField::PrivateAccessServer => app
            .private_access
            .focused_opt()
            .map(|profile| profile.server.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessPort => app
            .private_access
            .focused_opt()
            .map(|profile| profile.port.to_string())
            .unwrap_or_default(),
        SettingsField::PrivateAccessUsername => app
            .private_access
            .focused_opt()
            .map(|profile| profile.username.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessPassword => app
            .private_access
            .focused_opt()
            .map(|profile| profile.password.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessPasswordEnv => app
            .private_access
            .focused_opt()
            .map(|profile| profile.password_env.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessBridgeListen => app
            .private_access
            .focused_opt()
            .map(|profile| profile.bridge_listen.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessUseInternetProxy => app
            .private_access
            .focused_opt()
            .map(|profile| profile.use_internet_proxy.to_string())
            .unwrap_or_default(),
        SettingsField::PrivateAccessTlsVerify => app
            .private_access
            .focused_opt()
            .map(|profile| profile.tls_verify.to_string())
            .unwrap_or_default(),
    }
}

fn settings_field_display_value(app: &App, field: SettingsField) -> String {
    settings_field_value(app, field)
}

fn parse_positive<T>(value: &str) -> Result<T>
where
    T: std::str::FromStr + PartialOrd + From<u8>,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let parsed = value.parse::<T>().context("value must be a number")?;
    if parsed <= T::from(0) {
        bail!("value must be greater than 0");
    }
    Ok(parsed)
}

fn parse_bool_setting(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("value must be true or false"),
    }
}

fn normalize_http_connect_proxy(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let value = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("HTTP://"))
        .unwrap_or(value)
        .trim_end_matches('/');
    (!value.is_empty()).then(|| value.to_string())
}

type SonicwallHttpConnectSettings = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn sonicwall_http_connect_settings(
    use_internet_proxy: bool,
    system_proxy_server: &str,
    outbound_context: Option<String>,
    controller: &str,
    selector: Option<String>,
) -> SonicwallHttpConnectSettings {
    if !use_internet_proxy {
        return (None, None, None, None);
    }
    (
        normalize_http_connect_proxy(system_proxy_server),
        outbound_context,
        Some(controller.to_string()),
        selector,
    )
}

fn normalize_optional_setting(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_verification_targets_setting() -> String {
    DEFAULT_VERIFICATION_TARGETS
        .iter()
        .map(|(name, url)| format!("{name}={url}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_verification_targets(input: &str) -> Result<Vec<VerificationTarget>> {
    input
        .split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_verification_target)
        .collect()
}

fn parse_verification_target(input: &str) -> Result<VerificationTarget> {
    let (name, url) = input
        .split_once('=')
        .with_context(|| format!("verification target must be NAME=URL, got {input}"))?;
    let name = name.trim();
    let url = url.trim();
    if name.is_empty() {
        bail!("verification target name cannot be empty");
    }
    if url.is_empty() {
        bail!("verification target URL cannot be empty");
    }
    Ok(VerificationTarget {
        name: name.to_string(),
        url: url.to_string(),
    })
}

fn background_status_should_publish(status: &str) -> bool {
    status.starts_with("Auto-pick") || status.starts_with("Testing latency")
}

fn background_status_requires_selector_refresh(status: &str) -> bool {
    status.starts_with("Auto-pick switched") || status.starts_with("Auto-pick selected")
}

fn connection_is_direct(connection: &ConnectionInfo) -> bool {
    connection
        .chains
        .iter()
        .any(|chain| is_direct_chain_name(chain))
}

fn is_direct_chain_name(value: &str) -> bool {
    value.eq_ignore_ascii_case("direct") || value == "国内直连"
}

fn benchmark_job_kind_label(kind: &BenchmarkJobKind) -> &'static str {
    match kind {
        BenchmarkJobKind::Group => "group",
        BenchmarkJobKind::AutoSelect => "auto",
        BenchmarkJobKind::SingleNode { .. } => "single",
    }
}

fn next_clash_mode(current: Option<&str>, mode_list: &[String]) -> String {
    let modes = if mode_list.is_empty() {
        [GLOBAL_CLASH_MODE, DIRECT_CLASH_MODE, RULE_CLASH_MODE]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    } else {
        mode_list.to_vec()
    };
    let current_index = current.and_then(|value| modes.iter().position(|mode| mode == value));
    modes
        .get(current_index.map_or(0, |index| (index + 1) % modes.len()))
        .cloned()
        .unwrap_or_else(|| RULE_CLASH_MODE.to_string())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_exists(pid: u32) -> bool {
    let exists = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    exists && !process_is_zombie(pid)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_is_zombie(pid: u32) -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with('Z')
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn process_alive_via_ps(pid: u32) -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        // Cannot inspect the process list; assume alive so the caller still attempts the kill.
        return true;
    };
    let stat = String::from_utf8_lossy(&output.stdout);
    if !stat.trim().is_empty() {
        return !stat.trim_start().starts_with('Z');
    }
    // `ps -p` exits non-zero with empty stdout when the PID no longer exists. A real
    // inspection failure includes a diagnostic; stay conservative only in that case.
    !output.status.success() && !output.stderr.is_empty()
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return false;
    }
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    let mut exit_code = 0_u32;
    let alive = unsafe { GetExitCodeProcess(handle, &mut exit_code).is_ok() }
        && exit_code == STILL_ACTIVE.0 as u32;
    let _ = unsafe { CloseHandle(handle) };
    alive
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn process_exists(_pid: u32) -> bool {
    false
}

struct App {
    client: ApiClient,
    groups: Vec<ProxyGroup>,
    group_index: usize,
    internet_route_index: usize,
    member_index: usize,
    focus: Focus,
    left_pane_section: LeftPaneSection,
    intranet_detail_scroll: u16,
    expanded_intranet_sections: BTreeSet<String>,
    status: String,
    flash: Option<(String, Instant)>,
    benchmark_filter: String,
    benchmark_url: String,
    benchmark_timeout_ms: u64,
    benchmark_request_timeout: f64,
    benchmark_max_concurrency: usize,
    verify_targets: String,
    benchmarks: BTreeMap<String, BenchmarkSummary>,
    benchmark_jobs: Vec<BenchmarkJob>,
    latency_sort_mode: bool,
    last_single_node_benchmark: Option<(String, String, Instant)>,
    filter_input: Option<String>,
    bypass_input: Option<String>,
    bypass_entries: Vec<String>,
    auto_select_enabled: bool,
    auto_select_selector: Option<String>,
    auto_select_threshold_ms: u64,
    auto_select_interval: Duration,
    last_auto_select_benchmark: Option<Instant>,
    background_started_at_unix: u64,
    background_auto_pick: BackgroundAutoPickManager,
    benchmark_store: Option<BenchmarkStore>,
    state_store: Option<TuiStateStore>,
    bypass_rule_set_store: Option<BypassRuleSetStore>,
    latency_chart: Option<LatencyChartState>,
    clash_mode: Option<String>,
    clash_modes: Vec<String>,
    connections: ConnectionsSnapshot,
    connection_error: Option<String>,
    last_connection_refresh: Instant,
    show_connections: bool,
    show_help: bool,
    help_index: usize,
    onboarding_complete: bool,
    onboarding: Option<OnboardingState>,
    show_settings: bool,
    settings_index: usize,
    settings_edit: Option<SettingsEditState>,
    settings_error: Option<String>,
    subscription_refresh: Option<SubscriptionRefreshState>,
    system_proxy_config_path: PathBuf,
    system_proxy: SystemProxy,
    internet_tun: InternetTunTransaction,
    china_ip_routing_enabled: bool,
    china_ip_routing_explicit: bool,
    verify_job: Option<VerifyJob>,
    sing_box: ManagedSingBox,
    private_access: PrivateAccessRuntime,
    private_access_progress: Option<PrivateAccessProgressModal>,
    private_access_auth: Option<PrivateAccessAuthModal>,
}

struct SubscriptionRefreshState {
    request: SubscriptionRefreshRequest,
    interval: Duration,
    next_run: Instant,
    job: Option<SubscriptionRefreshJob>,
    last_report: Option<SubscriptionRefreshOutput>,
    last_error: Option<String>,
}

struct SubscriptionRefreshJob {
    receiver: mpsc::Receiver<SubscriptionRefreshEvent>,
    worker: JoinHandle<()>,
}

enum SubscriptionRefreshEvent {
    Finished(Result<SubscriptionRefreshOutput, String>),
}

struct VerifyJob {
    receiver: mpsc::Receiver<VerificationReport>,
    worker: JoinHandle<()>,
}

fn apply_internet_tun_persistence(state: &mut TuiRuntimeState, persisted: PersistedInternetTun) {
    state.tun_enabled = persisted.enabled();
    state.tun_auto_detect_interface_before_enable = persisted.restore_auto_detect_interface();
}

fn private_access_profile_settings_locked(profile: &PrivateAccessProfileRuntime) -> bool {
    profile.settings_locked()
}
impl SubscriptionRefreshState {
    fn from_options(options: TuiSubscriptionRefreshOptions) -> Result<Option<Self>> {
        if options.disabled || !options.input.exists() {
            return Ok(None);
        }
        if options.interval_days == 0 {
            bail!("--subscription-interval-days must be greater than 0");
        }
        let interval = Duration::from_secs(options.interval_days.saturating_mul(24 * 60 * 60));
        Ok(Some(Self {
            request: SubscriptionRefreshRequest {
                input: options.input,
                cache_path: options.cache_path,
                config_path: options.config_path.clone(),
                merged_path: options.config_path,
                replace_nodes: false,
                include_geosite_rules: options.include_geosite_rules,
                include_tun_mode: options.include_tun_mode,
                force: options.force,
                interval_days: options.interval_days,
            },
            interval,
            next_run: Instant::now(),
            job: None,
            last_report: None,
            last_error: None,
        }))
    }

    fn schedule_after(&mut self, delay: Duration) {
        self.next_run = Instant::now()
            .checked_add(delay)
            .unwrap_or_else(Instant::now);
    }
}

impl App {
    fn presentation_snapshot(&mut self) -> DashboardSnapshot<'_> {
        let flash = self.flash_message();
        let implicit_root_mode = self.implicit_root_mode();
        let implicit_current = self
            .implicit_root_group()
            .and_then(|root| root.current.as_deref());
        let internet_rows = self
            .displayed_group_names()
            .into_iter()
            .map(|name| {
                let current = self
                    .group_by_name(&name)
                    .and_then(|group| group.current.as_deref())
                    .unwrap_or("unset")
                    .to_string();
                let is_current = implicit_root_mode && implicit_current == Some(name.as_str());
                InternetRow {
                    name,
                    current,
                    is_current,
                }
            })
            .collect();
        let intranet_rows = self
            .private_access
            .profiles
            .iter()
            .map(|profile| IntranetRow {
                id: profile.id.clone(),
                state: profile.state.clone(),
                background: profile.background_pid.is_some(),
            })
            .collect();

        let displayed_members = self.displayed_members();
        let selected_group = self.selected_member_panel_group();
        let selected_benchmark = self.selected_benchmark();
        let candidate_rows = selected_group
            .map(|group| {
                displayed_members
                    .iter()
                    .map(|member| {
                        let result =
                            selected_benchmark.and_then(|summary| summary.find_result(member));
                        let (marker, tone) = match result {
                            Some(result) if !result.completed => {
                                (result.display_delay(), CandidateTone::Pending)
                            }
                            Some(result) if result.delay.is_some() => {
                                (result.display_delay(), CandidateTone::Success)
                            }
                            Some(result) => (result.display_delay(), CandidateTone::Error),
                            None => ("-".to_string(), CandidateTone::Missing),
                        };
                        CandidateRow {
                            name: member.clone(),
                            is_current: group.current.as_deref() == Some(member.as_str()),
                            marker,
                            tone,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let candidate_title = selected_group
            .map(|group| {
                format!(
                    "Candidates for {} [{}]",
                    group.name,
                    node_order_badge(self.latency_sort_mode)
                )
            })
            .unwrap_or_else(|| {
                format!("Candidates [{}]", node_order_badge(self.latency_sort_mode))
            });

        let showing_intranet_details = self.showing_intranet_details();
        let intranet_detail = if showing_intranet_details {
            self.private_access
                .focused_opt()
                .map(|profile| IntranetDetailSnapshot {
                    profile,
                    expanded_sections: &self.expanded_intranet_sections,
                    scroll: self.intranet_detail_scroll,
                    active: self.focus == Focus::Members,
                })
        } else {
            None
        };

        let mut selection_context = format!(
            "clash={}  Pick={}  filter='{}'",
            self.clash_mode_label(),
            pick_mode_badge(self.auto_select_enabled),
            self.benchmark_filter
        );
        if showing_intranet_details {
            selection_context.push_str("  Intranet details are shown in the right panel");
        }
        let footer = if let Some(input) = self.filter_input.as_ref() {
            StatusFooter::Filter(input.clone())
        } else if let Some(input) = self.bypass_input.as_ref() {
            StatusFooter::Bypass(input.clone())
        } else {
            StatusFooter::Status(self.status_line())
        };
        let status = StatusSnapshot {
            system_proxy_enabled: self.system_proxy.enabled(),
            tun_enabled: self.internet_tun.is_enabled(),
            selection_context,
            connections: self.connections_summary_line(),
            subscription: self.subscription_summary_line(),
            sing_box: self.sing_box_summary_line(),
            footer,
        };

        let connections = self.show_connections.then(|| ConnectionsPanelSnapshot {
            summary: self.connections_summary_line(),
            connections: &self.connections,
            error: self.connection_error.as_deref(),
        });
        let settings = self.show_settings.then(|| {
            let fields = visible_settings_fields(self);
            let rows = fields
                .iter()
                .map(|field| SettingRow {
                    label: settings_field_label(*field),
                    value: settings_field_display_value(self, *field),
                })
                .collect::<Vec<_>>();
            let editing = self
                .settings_edit
                .as_ref()
                .map(|edit| (settings_field_label(edit.field), edit.input.clone()));
            let error = self
                .settings_edit
                .as_ref()
                .and_then(|edit| edit.error.clone())
                .or_else(|| self.settings_error.clone());
            SettingsPanelSnapshot {
                selected: self.settings_index.min(rows.len().saturating_sub(1)),
                rows,
                editing,
                error,
            }
        });

        DashboardSnapshot {
            focus: self.focus,
            left_pane_section: self.left_pane_section,
            internet_rows,
            internet_selected: self.displayed_group_index(),
            intranet_rows,
            intranet_selected: self.private_access.focused_index,
            candidate_title,
            candidate_rows,
            candidate_selected: self.displayed_member_index(),
            intranet_detail,
            status,
            flash,
            latency_chart: self.latency_chart.as_ref(),
            connections,
            help_index: self.show_help.then_some(self.help_index),
            settings,
            onboarding: self.onboarding.as_ref(),
            private_access_progress: self.private_access_progress.as_ref(),
            private_access_auth: self.private_access_auth.as_ref(),
        }
    }

    fn new(
        client: ApiClient,
        benchmark_max_concurrency: usize,
        subscription_refresh_options: TuiSubscriptionRefreshOptions,
        sing_box_executable: PathBuf,
        keep_sing_box_running: bool,
        manage_sing_box: bool,
    ) -> Result<Self> {
        let state_store = TuiStateStore::new(default_tui_state_path());
        let existing_state_file = state_store.exists();
        let mut runtime_state = state_store.load()?;
        let onboarding_complete = runtime_state.onboarding_complete || existing_state_file;
        let system_proxy_config_path = subscription_refresh_options.config_path.clone();
        let system_proxy = SystemProxy::new(system_proxy_config_path.clone());
        let internet_tun = InternetTunTransaction::new(
            system_proxy_config_path.clone(),
            PersistedInternetTun::new(
                runtime_state.tun_enabled,
                runtime_state.tun_auto_detect_interface_before_enable,
            ),
        )?;
        let china_ip_routing_enabled =
            config_has_china_ip_routing(&system_proxy_config_path).unwrap_or(false);
        let subscription_refresh =
            SubscriptionRefreshState::from_options(subscription_refresh_options)?;
        let mut app = Self {
            client,
            groups: Vec::new(),
            group_index: 0,
            internet_route_index: 0,
            member_index: 0,
            focus: Focus::Groups,
            left_pane_section: LeftPaneSection::Internet,
            intranet_detail_scroll: 0,
            expanded_intranet_sections: BTreeSet::new(),
            status: String::from("Loading proxy groups..."),
            flash: None,
            benchmark_filter: String::new(),
            benchmark_url: String::from(DEFAULT_DELAY_TEST_URL),
            benchmark_timeout_ms: 5000,
            benchmark_request_timeout: 12.0,
            benchmark_max_concurrency,
            verify_targets: default_verification_targets_setting(),
            benchmarks: BTreeMap::new(),
            benchmark_jobs: Vec::new(),
            latency_sort_mode: false,
            last_single_node_benchmark: None,
            filter_input: None,
            bypass_input: None,
            bypass_entries: Vec::new(),
            auto_select_enabled: false,
            auto_select_selector: None,
            auto_select_threshold_ms: AUTO_SELECT_THRESHOLD_MS,
            auto_select_interval: AUTO_SELECT_INTERVAL,
            last_auto_select_benchmark: None,
            background_started_at_unix: current_unix_timestamp(),
            background_auto_pick: Default::default(),
            benchmark_store: Some(BenchmarkStore::open(default_benchmark_db_path())?),
            state_store: Some(state_store),
            bypass_rule_set_store: Some(BypassRuleSetStore::new(default_bypass_rule_set_path())),
            latency_chart: None,
            clash_mode: None,
            clash_modes: Vec::new(),
            connections: ConnectionsSnapshot::default(),
            connection_error: None,
            last_connection_refresh: Instant::now() - CONNECTION_REFRESH_INTERVAL,
            show_connections: false,
            show_help: false,
            help_index: 0,
            onboarding_complete,
            onboarding: (!onboarding_complete).then(|| OnboardingState {
                input: String::new(),
                message: String::from("Paste a subscription URL, or press s to skip setup."),
            }),
            show_settings: false,
            settings_index: 0,
            settings_edit: None,
            settings_error: None,
            subscription_refresh,
            system_proxy_config_path: system_proxy_config_path.clone(),
            system_proxy,
            internet_tun,
            china_ip_routing_enabled,
            china_ip_routing_explicit: false,
            verify_job: None,
            sing_box: ManagedSingBox::new(
                sing_box_executable,
                system_proxy_config_path,
                keep_sing_box_running,
            ),
            private_access: PrivateAccessRuntime::new()?,
            private_access_progress: None,
            private_access_auth: None,
        };
        app.apply_runtime_state(runtime_state.clone())?;
        if manage_sing_box {
            app.reconcile_persisted_tun_mode(&mut runtime_state)?;
            app.reconcile_persisted_china_ip_routing()?;
            app.ensure_private_access_tun_baseline()?;
            app.authorize_tun_elevation_if_needed()?;
            app.start_managed_sing_box()?;
        } else {
            wait_for_controller_ready(&app.client)
                .context("headless auto-pick could not reach the existing sing-box controller")?;
        }
        if let Err(error) = app.refresh() {
            let _ = app.shutdown_managed_sing_box();
            return Err(error);
        }
        app.restore_persisted_selections(&runtime_state)?;
        app.apply_runtime_state(runtime_state)?;
        app.save_bypass_rule_set()?;
        Ok(app)
    }

    fn reconcile_persisted_tun_mode(&mut self, runtime_state: &mut TuiRuntimeState) -> Result<()> {
        let state_store = self.state_store.clone();
        let mut persisted_runtime = self.runtime_state();
        self.internet_tun.reconcile(|tun| {
            apply_internet_tun_persistence(&mut persisted_runtime, tun);
            if let Some(store) = &state_store {
                store.save(&persisted_runtime)?;
            }
            Ok(())
        })?;
        apply_internet_tun_persistence(runtime_state, self.internet_tun.persisted());
        Ok(())
    }

    /// Re-applies an explicit China IP routing choice to the config when it drifted, e.g. after a
    /// subscription refresh regenerated the config without the geoip/geosite rule-sets.
    fn reconcile_persisted_china_ip_routing(&self) -> Result<()> {
        if !self.china_ip_routing_explicit || !self.system_proxy_config_path.exists() {
            return Ok(());
        }
        let in_config =
            config_has_china_ip_routing(&self.system_proxy_config_path).unwrap_or(false);
        if in_config != self.china_ip_routing_enabled {
            set_china_ip_routing(
                &self.system_proxy_config_path,
                self.china_ip_routing_enabled,
            )?;
        }
        Ok(())
    }

    /// Prompts for sudo credentials before the first elevated sing-box restart. `start_managed_sing_box`
    /// uses `sudo -n`, which never prompts, so a config that already has a TUN inbound would fail to
    /// launch sing-box once the cached sudo timestamp expires. Running `sudo -v` here re-authorizes
    /// interactively while the terminal is still in its normal (non-raw) mode.
    fn authorize_tun_elevation_if_needed(&self) -> Result<()> {
        if self.sing_box.startup_authorization_requirement()? == AuthorizationRequirement::None {
            return Ok(());
        }
        let status = Command::new("sudo")
            .arg("-v")
            .status()
            .context("failed to start sudo authorization for TUN mode")?;
        if !status.success() {
            bail!(
                "sudo authorization failed ({status}); TUN mode needs an elevated sing-box process"
            );
        }
        Ok(())
    }

    fn apply_runtime_state(&mut self, state: TuiRuntimeState) -> Result<()> {
        self.private_access
            .apply_state(&state, private_access_process_exists)?;
        if !self.private_access.is_configured() {
            self.left_pane_section = LeftPaneSection::Internet;
            self.intranet_detail_scroll = 0;
        }
        self.benchmark_filter = state.benchmark_filter;
        self.auto_select_enabled = state.auto_pick_enabled;
        self.auto_select_selector = state.auto_pick_selector;
        self.bypass_entries = state.bypass_entries;
        if let Some(value) = state.benchmark_url.filter(|value| !value.trim().is_empty()) {
            self.benchmark_url = value;
        }
        if let Some(value) = state.benchmark_timeout_ms.filter(|value| *value > 0) {
            self.benchmark_timeout_ms = value;
        }
        if let Some(value) = state.benchmark_request_timeout.filter(|value| *value > 0.0) {
            self.benchmark_request_timeout = value;
        }
        if let Some(value) = state.benchmark_max_concurrency.filter(|value| *value > 0) {
            self.benchmark_max_concurrency = value;
        }
        if let Some(value) = normalize_optional_setting(state.verify_targets) {
            self.verify_targets = value;
        }
        if let Some(value) = state.auto_select_threshold_ms.filter(|value| *value > 0) {
            self.auto_select_threshold_ms = value;
        }
        if let Some(value) = state.auto_select_interval_secs.filter(|value| *value > 0) {
            self.auto_select_interval = Duration::from_secs(value);
        }
        if let Some(value) = state
            .system_proxy_server
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            self.system_proxy
                .restore_server(value, state.system_proxy_server_override);
        }
        if let Some(value) = state.china_ip_routing_enabled {
            self.china_ip_routing_enabled = value;
            self.china_ip_routing_explicit = true;
        }
        self.last_auto_select_benchmark = None;
        if let Some(group) = self.selected_group()
            && let Some(node) = state.current_selected_nodes.get(&group.name)
        {
            let node = node.clone();
            self.sync_selection_to_member_name(&node);
        }
        self.sync_selection_to_displayed_members();
        Ok(())
    }

    fn apply_background_auto_pick_config(&mut self, config: AutoPickConfig) {
        let before = self.auto_pick_runtime_signature();
        self.benchmark_filter = config.filter;
        self.auto_select_enabled = config.enabled;
        self.auto_select_selector = config.selector;
        if !config.benchmark_url.trim().is_empty() {
            self.benchmark_url = config.benchmark_url;
        }
        if config.timeout_ms > 0 {
            self.benchmark_timeout_ms = config.timeout_ms;
        }
        if config.request_timeout > 0.0 {
            self.benchmark_request_timeout = config.request_timeout;
        }
        if config.max_concurrency > 0 {
            self.benchmark_max_concurrency = config.max_concurrency;
        }
        if config.threshold_ms > 0 {
            self.auto_select_threshold_ms = config.threshold_ms;
        }
        if config.interval_secs > 0 {
            self.auto_select_interval = Duration::from_secs(config.interval_secs);
        }
        if before != self.auto_pick_runtime_signature() {
            self.last_auto_select_benchmark = None;
        }
    }

    fn auto_pick_runtime_signature(
        &self,
    ) -> (
        bool,
        Option<String>,
        String,
        String,
        u64,
        u64,
        u64,
        u64,
        usize,
    ) {
        (
            self.auto_select_enabled,
            self.auto_select_selector.clone(),
            self.benchmark_filter.clone(),
            self.benchmark_url.clone(),
            self.benchmark_timeout_ms,
            self.benchmark_request_timeout.to_bits(),
            self.auto_select_threshold_ms,
            self.auto_select_interval.as_secs(),
            self.benchmark_max_concurrency,
        )
    }

    fn runtime_state(&self) -> TuiRuntimeState {
        let persisted_tun = self.internet_tun.persisted();
        TuiRuntimeState {
            benchmark_filter: self.benchmark_filter.clone(),
            auto_pick_enabled: self.auto_select_enabled,
            auto_pick_selector: self.auto_select_selector.clone(),
            current_selected_nodes: self
                .groups
                .iter()
                .filter_map(|group| {
                    group
                        .current
                        .as_ref()
                        .map(|current| (group.name.clone(), current.clone()))
                })
                .collect(),
            bypass_entries: self.bypass_entries.clone(),
            onboarding_complete: self.onboarding_complete,
            benchmark_url: Some(self.benchmark_url.clone()),
            benchmark_timeout_ms: Some(self.benchmark_timeout_ms),
            benchmark_request_timeout: Some(self.benchmark_request_timeout),
            benchmark_max_concurrency: Some(self.benchmark_max_concurrency),
            verify_targets: normalize_optional_setting(Some(self.verify_targets.clone())),
            auto_select_threshold_ms: Some(self.auto_select_threshold_ms),
            auto_select_interval_secs: Some(self.auto_select_interval.as_secs()),
            system_proxy_server: Some(self.system_proxy.server().to_string()),
            system_proxy_server_override: self.system_proxy.server_is_overridden(),
            tun_enabled: persisted_tun.enabled(),
            tun_auto_detect_interface_before_enable: persisted_tun.restore_auto_detect_interface(),
            china_ip_routing_enabled: self
                .china_ip_routing_explicit
                .then_some(self.china_ip_routing_enabled),
            private_access_profiles: self.private_access.runtime_states(process_exists),
        }
    }

    fn auto_pick_config(&self) -> AutoPickConfig {
        AutoPickConfig {
            enabled: self.auto_select_enabled,
            selector: self.auto_select_selector.clone(),
            filter: self.benchmark_filter.clone(),
            benchmark_url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            threshold_ms: self.auto_select_threshold_ms,
            interval_secs: self.auto_select_interval.as_secs(),
        }
    }

    fn background_launch_spec(&self) -> BackgroundLaunchSpec {
        BackgroundLaunchSpec::new(
            self.client.base_url.clone(),
            self.system_proxy_config_path.clone(),
            self.benchmark_max_concurrency,
        )
    }

    fn save_runtime_state(&self) -> Result<()> {
        let Some(store) = &self.state_store else {
            return Ok(());
        };
        store.save(&self.runtime_state())
    }

    fn persisted_selection_restore_plan(&self, state: &TuiRuntimeState) -> Vec<(String, String)> {
        self.groups
            .iter()
            .filter(|group| group.kind.eq_ignore_ascii_case("selector"))
            .filter_map(|group| {
                let node = state.current_selected_nodes.get(&group.name)?;
                if group.current.as_ref() == Some(node) {
                    return None;
                }
                if !group.members.iter().any(|member| member == node) {
                    return None;
                }
                Some((group.name.clone(), node.clone()))
            })
            .collect()
    }

    fn restore_persisted_selections(&mut self, state: &TuiRuntimeState) -> Result<()> {
        let plan = self.persisted_selection_restore_plan(state);
        if plan.is_empty() {
            return Ok(());
        }

        let mut restored = 0usize;
        let mut failures = Vec::new();
        for (group, node) in plan {
            match self.client.switch_proxy(&group, &node) {
                Ok(()) => restored += 1,
                Err(error) => failures.push(format!("{group} -> {node}: {error}")),
            }
        }

        if restored > 0 {
            if REFRESH_DEBOUNCE > Duration::ZERO {
                std::thread::sleep(REFRESH_DEBOUNCE);
            }
            self.refresh()?;
        }

        if failures.is_empty() {
            if restored > 0 {
                self.set_status_only(format!("Restored {restored} saved selector selection(s)"));
            }
        } else {
            let detail = truncate_for_width(&failures.join("; "), 90);
            self.set_status_only(format!(
                "Restored {restored} saved selector selection(s); failed: {detail}"
            ));
        }

        Ok(())
    }

    fn save_bypass_rule_set(&self) -> Result<()> {
        let Some(store) = &self.bypass_rule_set_store else {
            return Ok(());
        };
        store.save(&self.bypass_entries)
    }

    fn selected_group(&self) -> Option<&ProxyGroup> {
        if self.implicit_root_mode() {
            return self
                .selected_root_choice_name()
                .and_then(|name| self.group_by_name(&name));
        }
        self.groups.get(self.group_index)
    }

    fn internet_outbound_context(&self) -> Option<String> {
        let mut current = self
            .implicit_root_group()
            .or_else(|| self.selected_group())?;
        let mut chain = vec![current.name.clone()];
        let mut visited = BTreeSet::new();
        visited.insert(current.name.clone());
        loop {
            let Some(selected) = current.current.as_deref() else {
                break;
            };
            chain.push(selected.to_string());
            let Some(next) = self.group_by_name(selected) else {
                break;
            };
            if !visited.insert(next.name.clone()) {
                chain.push("(cycle)".to_string());
                break;
            }
            current = next;
        }
        (!chain.is_empty()).then(|| chain.join(" -> "))
    }

    fn internet_outbound_root_selector(&self) -> Option<String> {
        self.implicit_root_group()
            .or_else(|| self.selected_group())
            .map(|group| group.name.clone())
    }

    fn group_by_name(&self, name: &str) -> Option<&ProxyGroup> {
        self.groups.iter().find(|group| group.name == name)
    }

    fn implicit_root_group(&self) -> Option<&ProxyGroup> {
        let root = self.group_by_name(DEFAULT_SELECTOR_TAG)?;
        let internet_route_group_count = root
            .members
            .iter()
            .filter(|member| self.is_internet_route_child_group(member))
            .count();
        if internet_route_group_count >= 1 {
            Some(root)
        } else {
            None
        }
    }

    fn implicit_root_mode(&self) -> bool {
        self.implicit_root_group().is_some()
    }

    fn displayed_group_names(&self) -> Vec<String> {
        if let Some(root) = self.implicit_root_group() {
            return self.internet_route_child_group_names(root);
        }
        self.groups.iter().map(|group| group.name.clone()).collect()
    }

    fn displayed_group_index(&self) -> usize {
        if self.implicit_root_mode() {
            self.internet_route_index
        } else {
            self.group_index
        }
    }

    fn showing_intranet_details(&self) -> bool {
        self.left_pane_section == LeftPaneSection::Intranet && self.private_access.is_configured()
    }

    fn intranet_detail_section_key(profile_id: &str, section: IntranetDetailSection) -> String {
        format!("{profile_id}:{}", section.key())
    }

    fn intranet_detail_view(&self, profile: &PrivateAccessProfileRuntime) -> IntranetDetailView {
        private_access_detail_view(profile, |section| {
            self.expanded_intranet_sections
                .contains(&Self::intranet_detail_section_key(&profile.id, section))
        })
    }

    fn intranet_detail_line_count(&self) -> usize {
        self.private_access
            .focused_opt()
            .map(|profile| self.intranet_detail_view(profile).lines.len())
            .unwrap_or(0)
    }

    fn toggle_intranet_detail_section(&mut self) {
        let Some(profile) = self.private_access.focused_opt() else {
            return;
        };
        let profile_id = profile.id.clone();
        let view = self.intranet_detail_view(profile);
        let cursor = self.intranet_detail_scroll as usize;
        let Some(range) = view
            .sections
            .iter()
            .find(|range| range.foldable && cursor >= range.start && cursor < range.end)
            .or_else(|| {
                view.sections
                    .iter()
                    .find(|range| range.foldable && range.start >= cursor)
            })
            .or_else(|| view.sections.iter().rev().find(|range| range.foldable))
            .copied()
        else {
            self.set_status_only("No detail section has more than 10 items");
            return;
        };
        let key = Self::intranet_detail_section_key(&profile_id, range.section);
        let expanded = if self.expanded_intranet_sections.remove(&key) {
            false
        } else {
            self.expanded_intranet_sections.insert(key);
            true
        };
        self.intranet_detail_scroll = range.start as u16;
        self.set_status_only(format!(
            "{} {} section for {}",
            if expanded { "Expanded" } else { "Folded" },
            range.section.key(),
            profile_id
        ));
    }

    fn selected_root_choice_name(&self) -> Option<String> {
        self.implicit_root_group().and_then(|root| {
            self.internet_route_child_group_names(root)
                .into_iter()
                .nth(self.internet_route_index)
        })
    }

    fn internet_route_child_group_names(&self, root: &ProxyGroup) -> Vec<String> {
        root.members
            .iter()
            .filter(|member| self.is_internet_route_child_group(member))
            .cloned()
            .collect()
    }

    fn is_internet_route_child_group(&self, member: &str) -> bool {
        self.group_by_name(member)
            .is_some_and(|group| group.kind.eq_ignore_ascii_case("selector"))
    }

    fn implicit_root_parent_switch_for_group(&self, group_name: &str) -> Option<(String, String)> {
        let root = self.implicit_root_group()?;
        if root.current.as_deref() == Some(group_name) {
            return None;
        }
        if self
            .internet_route_child_group_names(root)
            .iter()
            .any(|route_group| route_group == group_name)
        {
            return Some((root.name.clone(), group_name.to_string()));
        }
        None
    }

    fn selected_member_panel_group(&self) -> Option<&ProxyGroup> {
        if self.showing_intranet_details() {
            return None;
        }
        if self.implicit_root_mode() {
            let choice = self.selected_root_choice_name()?;
            return self.group_by_name(&choice);
        }
        self.selected_group()
    }

    fn selected_member_panel_is_manual_selector(&self) -> bool {
        self.selected_member_panel_group()
            .is_some_and(|group| group.kind.eq_ignore_ascii_case("selector"))
    }

    fn selected_benchmark(&self) -> Option<&BenchmarkSummary> {
        let group = self.selected_member_panel_group()?;
        self.benchmarks.get(&group.name)
    }

    fn member_matches_filter(&self, member: &str) -> bool {
        matches_filter(member, &self.benchmark_filter)
    }

    fn benchmark_candidates_for_group(&self, group: &ProxyGroup) -> Vec<String> {
        group
            .members
            .iter()
            .filter(|member| self.member_matches_filter(member))
            .cloned()
            .collect()
    }

    fn displayed_members(&self) -> Vec<String> {
        let Some(group) = self.selected_group() else {
            return Vec::new();
        };
        let group = self.selected_member_panel_group().unwrap_or(group);
        let Some(summary) = self.selected_benchmark() else {
            return group
                .members
                .iter()
                .filter(|member| self.member_matches_filter(member))
                .cloned()
                .collect();
        };
        if !self.latency_sort_mode {
            return group
                .members
                .iter()
                .filter(|member| self.member_matches_filter(member))
                .cloned()
                .collect();
        }

        let mut successes = Vec::new();
        let mut pending_or_untested = Vec::new();
        for (index, member) in group.members.iter().enumerate() {
            if !self.member_matches_filter(member) {
                continue;
            }
            match summary.find_result(member) {
                Some(result) if result.completed && result.delay.is_none() => {}
                Some(result) if result.completed => {
                    successes.push((result.delay.unwrap_or(u64::MAX), index, member.clone()))
                }
                _ => pending_or_untested.push((index, member.clone())),
            }
        }
        successes.sort_by_key(|(delay, index, _)| (*delay, *index));
        let mut out = successes
            .into_iter()
            .map(|(_, _, member)| member)
            .collect::<Vec<_>>();
        out.extend(pending_or_untested.into_iter().map(|(_, member)| member));
        out
    }

    fn displayed_member_index(&self) -> Option<usize> {
        let members = self.displayed_members();
        let current = self
            .selected_member_panel_group()?
            .members
            .get(self.member_index)?;
        members.iter().position(|member| member == current)
    }

    fn sync_selection_to_member_name(&mut self, name: &str) {
        if let Some(group) = self.selected_member_panel_group()
            && let Some(index) = group.members.iter().position(|member| member == name)
        {
            self.member_index = index;
        }
    }

    fn sync_selection_to_displayed_members(&mut self) {
        let displayed = self.displayed_members();
        if displayed.is_empty() {
            return;
        }

        let current = self
            .selected_member_panel_group()
            .and_then(|group| group.members.get(self.member_index))
            .cloned();
        if current
            .as_ref()
            .is_some_and(|member| displayed.iter().any(|item| item == member))
        {
            return;
        }

        if let Some(first) = displayed.first() {
            let next = first.clone();
            self.sync_selection_to_member_name(&next);
        }
    }

    fn status_line(&self) -> String {
        self.status.clone()
    }

    fn sing_box_summary_line(&self) -> String {
        format!("sing-box: {}", self.sing_box.diagnostics())
    }

    fn connections_summary_line(&self) -> String {
        if let Some(error) = &self.connection_error {
            return format!("connections unavailable: {}", truncate_for_width(error, 80));
        }

        let direct_count = self
            .connections
            .connections
            .iter()
            .filter(|connection| connection_is_direct(connection))
            .count();
        let active_count = self.connections.connections.len();
        let proxied_count = active_count.saturating_sub(direct_count);
        format!(
            "connections active={} proxy={} direct={} up={} down={}",
            active_count,
            proxied_count,
            direct_count,
            format_bytes_opt(self.connections.upload_total),
            format_bytes_opt(self.connections.download_total)
        )
    }

    fn subscription_summary_line(&self) -> String {
        let Some(state) = &self.subscription_refresh else {
            return "subscriptions: disabled or no .suburl".to_string();
        };
        if state.job.is_some() {
            return format!(
                "subscriptions: refreshing {} -> {}",
                state.request.input.display(),
                state.request.merged_path.display()
            );
        }
        if let Some(error) = &state.last_error {
            return format!(
                "subscriptions: error: {}  retry in {}",
                truncate_for_width(error, 72),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now()))
            );
        }
        if let Some(report) = &state.last_report {
            return format!(
                "subscriptions: {}  next in {}  reload sing-box to apply",
                subscription_report_badge(report),
                format_duration_badge(state.next_run.saturating_duration_since(Instant::now()))
            );
        }
        format!(
            "subscriptions: pending first refresh from {}",
            state.request.input.display()
        )
    }

    fn maybe_start_subscription_refresh(&mut self) {
        let Some(state) = self.subscription_refresh.as_mut() else {
            return;
        };
        if state.job.is_some() || Instant::now() < state.next_run {
            return;
        }

        self.start_subscription_refresh_job(false, "Refreshing subscriptions in background...");
    }

    fn start_manual_subscription_refresh(&mut self) {
        if self.subscription_refresh.is_none() {
            self.set_status_only("Subscription refresh is disabled or .suburl was not found");
            return;
        }
        let started = self.start_subscription_refresh_job(
            true,
            "Manually refreshing subscriptions in background...",
        );
        if !started {
            self.set_status_only("Subscription refresh is already running");
        }
    }

    fn start_subscription_refresh_job(&mut self, force: bool, status: &str) -> bool {
        let Some(state) = self.subscription_refresh.as_mut() else {
            return false;
        };
        if state.job.is_some() {
            return false;
        }

        let mut request = state.request.clone();
        request.force = force || state.request.force;
        state.request.force = false;
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = refresh_subscriptions(&request).map_err(|error| error.to_string());
            let _ = tx.send(SubscriptionRefreshEvent::Finished(result));
        });
        state.job = Some(SubscriptionRefreshJob {
            receiver: rx,
            worker,
        });
        state.last_error = None;
        self.set_status_only(status);
        true
    }

    fn poll_subscription_refresh_updates(&mut self) -> Result<()> {
        let Some(state) = self.subscription_refresh.as_mut() else {
            return Ok(());
        };
        let Some(job) = state.job.as_ref() else {
            return Ok(());
        };

        let event = match job.receiver.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => SubscriptionRefreshEvent::Finished(Err(
                "subscription refresh worker disconnected".to_string(),
            )),
        };

        let job = state.job.take().expect("subscription refresh job exists");
        let _ = job.worker.join();
        match event {
            SubscriptionRefreshEvent::Finished(Ok(report)) => {
                state.schedule_after(state.interval);
                state.last_error = None;
                state.last_report = Some(report.clone());
                self.set_status_only(format!(
                    "Subscription refresh updated config: {}; reload/restart sing-box to apply",
                    subscription_report_badge(&report)
                ));
            }
            SubscriptionRefreshEvent::Finished(Err(error)) => {
                state.schedule_after(SUBSCRIPTION_REFRESH_RETRY_INTERVAL);
                state.last_error = Some(error.clone());
                self.set_status_only(format!(
                    "Subscription refresh failed: {}; retry in {}",
                    truncate_for_width(&error, 80),
                    format_duration_badge(SUBSCRIPTION_REFRESH_RETRY_INTERVAL)
                ));
            }
        }
        Ok(())
    }

    fn maybe_refresh_connections(&mut self) {
        if self.last_connection_refresh.elapsed() < CONNECTION_REFRESH_INTERVAL {
            return;
        }
        self.last_connection_refresh = Instant::now();
        match self.client.fetch_connections() {
            Ok(connections) => {
                self.connections = connections;
                self.connection_error = None;
            }
            Err(error) => {
                self.connection_error = Some(error.to_string());
            }
        }
    }

    fn poll_background_auto_pick_status(&mut self) -> Result<()> {
        if !self.background_worker_management_enabled() {
            return Ok(());
        }
        let config = self.auto_pick_config();
        let launch = self.background_launch_spec();
        let Some(event) =
            self.background_auto_pick
                .poll(self.auto_select_enabled, &config, &launch)?
        else {
            return Ok(());
        };
        match event {
            BackgroundPollEvent::Update(update) => {
                self.apply_background_latency_snapshot(update.latency.as_ref());
                if let Some(status) = update.status {
                    if background_status_requires_selector_refresh(&status) {
                        self.refresh()?;
                    }
                    self.set_status_only(format!("Auto-pick worker: {status}"));
                }
            }
            BackgroundPollEvent::Retry(error) => self.set_status_only(format!(
                "Auto-pick worker TCP error; process is still alive, retrying: {error}"
            )),
            BackgroundPollEvent::Exited(error) => {
                self.set_status_only(format!("Auto-pick worker exited after TCP error: {error}"))
            }
            BackgroundPollEvent::Restarted(worker) => self.set_status_only(format!(
                "Auto-pick background worker {} pid {} after previous worker exited",
                worker.label(),
                worker.pid()
            )),
            BackgroundPollEvent::Ensured(worker) => self.set_status_only(format!(
                "Auto-pick background worker {} pid {}",
                worker.label(),
                worker.pid()
            )),
        }
        Ok(())
    }

    fn apply_background_latency_snapshot(&mut self, latency: Option<&BackgroundLatencySnapshot>) {
        let Some(latency) = latency else {
            return;
        };
        if latency.pattern != self.benchmark_filter {
            return;
        }
        if self.benchmark_jobs.iter().any(|job| {
            job.group == latency.selector && !matches!(job.kind, BenchmarkJobKind::AutoSelect)
        }) {
            return;
        }
        let summary = self
            .benchmarks
            .entry(latency.selector.clone())
            .or_insert_with(|| BenchmarkSummary::empty(latency.selector.clone()));
        summary.current = latency.current.clone();
        summary.pattern = latency.pattern.clone();
        summary.url = latency.url.clone();
        summary.timeout_ms = latency.timeout_ms;
        summary.max_concurrency = latency.max_concurrency;
        for result in &latency.results {
            summary.update_result(BenchmarkResult {
                name: result.name.clone(),
                delay: result.delay,
                completed: result.completed,
            });
        }
    }

    fn set_status_only(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.flash = None;
    }

    fn set_status_with_flash(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.flash = Some((self.status.clone(), Instant::now()));
    }

    fn set_switch_status(&mut self, group: &str, member: &str) {
        self.set_status_only(format!("Switched {} to {}", group, member));
    }

    fn clash_mode_label(&self) -> &str {
        self.clash_mode.as_deref().unwrap_or("unknown")
    }

    fn flash_message(&mut self) -> Option<String> {
        let (message, since) = self.flash.as_ref()?;
        if since.elapsed() > Duration::from_secs(2) {
            self.flash = None;
            return None;
        }
        Some(message.clone())
    }

    fn handle_key(&mut self, code: KeyCode) -> Result<bool> {
        if self.private_access_auth.is_some() {
            return self.handle_private_access_auth_key(code);
        }
        if self.private_access_progress.is_some() {
            match code {
                KeyCode::Esc | KeyCode::Enter => self.private_access_progress = None,
                KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
            return Ok(true);
        }
        if self.onboarding.is_some() {
            return self.handle_onboarding_key(code);
        }
        if self.show_settings {
            return self.handle_settings_key(code);
        }
        if self.filter_input.is_some() {
            return self.handle_filter_input_key(code);
        }
        if self.bypass_input.is_some() {
            return self.handle_bypass_input_key(code);
        }
        if self.show_help {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') => {
                    self.show_help = false;
                    self.set_status_only("Help closed");
                }
                KeyCode::Down | KeyCode::Char('j') => self.move_help_next(),
                KeyCode::Up | KeyCode::Char('k') => self.move_help_previous(),
                KeyCode::Char('g') => self.help_index = 0,
                KeyCode::Char('G') => self.help_index = help_binding_count().saturating_sub(1),
                KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
            return Ok(true);
        }
        if self.show_connections {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('c') => {
                    self.show_connections = false;
                    self.set_status_only("Connection details closed");
                }
                KeyCode::Char('r') => {
                    self.last_connection_refresh = Instant::now() - CONNECTION_REFRESH_INTERVAL;
                    self.maybe_refresh_connections();
                    self.set_status_only("Connection details refreshed");
                }
                KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
            return Ok(true);
        }
        if self.latency_chart.is_some() {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('i') => {
                    self.latency_chart = None;
                    self.set_status_only("Latency chart closed");
                }
                KeyCode::Char('z') => self.zoom_latency_chart_in(),
                KeyCode::Char('Z') => self.zoom_latency_chart_out(),
                KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
            return Ok(true);
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Groups => Focus::Members,
                    Focus::Members => Focus::Groups,
                };
            }
            KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Members,
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Groups,
            KeyCode::Down | KeyCode::Char('j') => self.move_next(),
            KeyCode::Up | KeyCode::Char('k') => self.move_previous(),
            KeyCode::Char('g') => self.move_first(),
            KeyCode::Char('G') => self.move_last(),
            KeyCode::Char('r') => self.refresh()?,
            KeyCode::Char('u') => self.start_manual_subscription_refresh(),
            KeyCode::Char('T') => self.start_group_benchmark()?,
            KeyCode::Char('t') => self.start_member_benchmark()?,
            KeyCode::Char('s') => self.toggle_latency_sort_mode(),
            KeyCode::Char('a') => self.toggle_auto_select()?,
            KeyCode::Char('m') => self.cycle_clash_mode()?,
            KeyCode::Char('b') => self.open_bypass_modal(),
            KeyCode::Char('B') => return self.keep_sing_box_running_in_background(),
            KeyCode::Char('p') => self.set_system_proxy(),
            KeyCode::Char('\\') => self.toggle_tun_mode(),
            KeyCode::Char('i') => self.open_latency_chart()?,
            KeyCode::Char('c') => self.open_connections_panel(),
            KeyCode::Char('v') => self.start_verify(),
            KeyCode::Char('V') => self.toggle_private_access_with_progress()?,
            KeyCode::Char('o') => self.open_settings_panel(),
            KeyCode::Char('?') => self.open_help_panel(),
            KeyCode::Char('/') => self.open_benchmark_filter_modal(),
            KeyCode::Char(' ') => self.activate_selection()?,
            KeyCode::Enter if self.focus == Focus::Members && self.showing_intranet_details() => {
                self.toggle_intranet_detail_section();
            }
            KeyCode::Enter => {}
            _ => {}
        }
        Ok(true)
    }

    fn handle_mouse(&mut self, kind: MouseEventKind) {
        if !self.show_help {
            return;
        }
        match kind {
            MouseEventKind::ScrollDown => self.move_help_next(),
            MouseEventKind::ScrollUp => self.move_help_previous(),
            _ => {}
        }
    }

    fn selected_member_name(&self) -> Option<String> {
        self.selected_group()?
            .members
            .get(self.member_index)
            .cloned()
    }

    fn open_connections_panel(&mut self) {
        self.show_connections = true;
        self.set_status_only("Showing active connections");
    }

    fn open_help_panel(&mut self) {
        self.show_help = true;
        self.flash = None;
        self.set_status_only("Showing help");
    }

    fn open_settings_panel(&mut self) {
        self.show_settings = true;
        self.settings_edit = None;
        let field_count = visible_settings_fields(self).len();
        self.settings_index = self.settings_index.min(field_count.saturating_sub(1));
        self.flash = None;
        self.set_status_only("Showing settings");
    }

    fn handle_onboarding_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc => {
                self.onboarding = None;
                self.set_status_only("First run setup postponed");
            }
            KeyCode::Char('s') => {
                self.onboarding_complete = true;
                self.onboarding = None;
                self.save_runtime_state()?;
                self.set_status_only("First run setup skipped");
            }
            KeyCode::Enter => self.finish_onboarding_with_subscription()?,
            KeyCode::Backspace => {
                if let Some(onboarding) = &mut self.onboarding {
                    onboarding.input.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(onboarding) = &mut self.onboarding {
                    onboarding.input.push(ch);
                }
            }
            _ => {}
        }
        Ok(true)
    }

    fn finish_onboarding_with_subscription(&mut self) -> Result<()> {
        let Some(onboarding) = &mut self.onboarding else {
            return Ok(());
        };
        let url = onboarding.input.trim();
        if url.is_empty() {
            onboarding.message = "Paste a subscription URL first, or press s to skip.".to_string();
            return Ok(());
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            onboarding.message = "Subscription URL must start with http:// or https://".to_string();
            return Ok(());
        }
        let line = format!("default = {url}\n");
        let path = PathBuf::from(DEFAULT_SUBSCRIPTION_SOURCE_PATH);
        if path.exists() {
            let existing = fs::read_to_string(&path).unwrap_or_default();
            if !existing.contains(url) {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("failed to open {}", path.display()))?;
                use std::io::Write;
                if !existing.ends_with('\n') && !existing.is_empty() {
                    writeln!(file)
                        .with_context(|| format!("failed to write {}", path.display()))?;
                }
                file.write_all(line.as_bytes())
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
        } else {
            fs::write(&path, line)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
        self.onboarding = None;
        self.onboarding_complete = true;
        self.save_runtime_state()?;
        self.set_status_only("First run setup saved .suburl; press u to refresh subscriptions");
        Ok(())
    }

    fn handle_settings_key(&mut self, code: KeyCode) -> Result<bool> {
        if self.settings_edit.is_some() {
            return self.handle_settings_edit_key(code);
        }
        let fields = visible_settings_fields(self);
        self.settings_index = self.settings_index.min(fields.len().saturating_sub(1));
        match code {
            KeyCode::Esc | KeyCode::Char('o') => {
                self.show_settings = false;
                self.settings_error = None;
                self.set_status_only("Settings closed");
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_error = None;
                self.settings_index = (self.settings_index + 1).min(fields.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_error = None;
                self.settings_index = self.settings_index.saturating_sub(1);
            }
            KeyCode::Enter => {
                let field = fields[self.settings_index];
                self.settings_error = None;
                self.settings_edit = Some(SettingsEditState {
                    field,
                    input: settings_field_value(self, field),
                    error: None,
                });
            }
            KeyCode::Char('q') => return Ok(false),
            _ => {}
        }
        Ok(true)
    }

    fn handle_settings_edit_key(&mut self, code: KeyCode) -> Result<bool> {
        let Some(edit) = self.settings_edit.as_mut() else {
            return Ok(true);
        };
        match code {
            KeyCode::Esc => {
                self.settings_edit = None;
                self.settings_error = None;
            }
            KeyCode::Enter => {
                let edit = self.settings_edit.take().expect("settings edit exists");
                if let Err(error) = self.apply_settings_value(edit.field, edit.input.clone()) {
                    let message = error.to_string();
                    self.settings_edit = Some(SettingsEditState {
                        error: Some(message),
                        ..edit
                    });
                }
            }
            KeyCode::Backspace => {
                edit.input.pop();
                edit.error = None;
            }
            KeyCode::Char(ch) => {
                edit.input.push(ch);
                edit.error = None;
            }
            _ => {}
        }
        Ok(true)
    }

    fn apply_settings_value(&mut self, field: SettingsField, input: String) -> Result<()> {
        if is_private_access_settings_field(field) && !self.private_access.is_configured() {
            bail!("Private Access is not configured");
        }
        let value = input.trim();
        match field {
            SettingsField::BenchmarkUrl => {
                if value.is_empty() {
                    bail!("latency URL cannot be empty");
                }
                self.benchmark_url = value.to_string();
            }
            SettingsField::BenchmarkTimeoutMs => self.benchmark_timeout_ms = parse_positive(value)?,
            SettingsField::RequestTimeoutSec => {
                self.benchmark_request_timeout = value
                    .parse::<f64>()
                    .context("request timeout must be a number")?;
                if self.benchmark_request_timeout <= 0.0 {
                    bail!("request timeout must be greater than 0");
                }
            }
            SettingsField::MaxConcurrency => {
                self.benchmark_max_concurrency = parse_positive(value)?
            }
            SettingsField::VerifyTargets => {
                if !value.is_empty() {
                    parse_verification_targets(value)?;
                }
                self.verify_targets = value.to_string();
            }
            SettingsField::AutoPickThresholdMs => {
                self.auto_select_threshold_ms = parse_positive(value)?
            }
            SettingsField::AutoPickIntervalSec => {
                let seconds: u64 = parse_positive(value)?;
                self.auto_select_interval = Duration::from_secs(seconds);
            }
            SettingsField::SystemProxyServer => {
                if value.is_empty() {
                    bail!("system proxy server cannot be empty");
                }
                self.system_proxy.override_server(value.to_string());
            }
            SettingsField::ChinaIpRouting => {
                let enable = parse_bool_setting(value)?;
                if enable {
                    let ruleset_dir = china_ip_routing_ruleset_dir(&self.system_proxy_config_path);
                    let proxy_server = self.system_proxy.server().to_string();
                    self.client
                        .runtime
                        .block_on(download_china_ip_routing_rulesets(
                            Some(&proxy_server),
                            &ruleset_dir,
                        ))?;
                }
                let changed = set_china_ip_routing(&self.system_proxy_config_path, enable)?;
                self.china_ip_routing_enabled = enable;
                self.china_ip_routing_explicit = true;
                self.save_runtime_state()?;
                if changed {
                    let receipt = self.restart_managed_sing_box()?;
                    let label = if enable { "enabled" } else { "disabled" };
                    match receipt.observe_controller(&self.client) {
                        Ok(()) => self.set_status_with_flash(format!(
                            "China IP routing {label}; sing-box restarted"
                        )),
                        Err(error) => self.set_status_with_flash(format!(
                            "China IP routing {label}; controller not ready: {}",
                            truncate_for_width(&format!("{error:#}"), 60)
                        )),
                    }
                } else {
                    self.set_status_with_flash(format!(
                        "China IP routing already {}",
                        if enable { "enabled" } else { "disabled" }
                    ));
                }
                return Ok(());
            }
            SettingsField::PrivateAccessProfile => {
                self.private_access.set_focus_by_id(value)?;
            }
            SettingsField::PrivateAccessManifestPath => {
                if private_access_profile_settings_locked(self.private_access.focused()) {
                    bail!("disconnect Private Access before changing service manifest");
                }
                let profile_id = self.private_access.focused().id.clone();
                let manifest_path = normalize_optional_setting(Some(value.to_string()));
                let manifest = load_manifest_for_profile(&profile_id, manifest_path.as_deref())?;
                let focused = self.private_access.focused_mut();
                focused.manifest_path = manifest_path;
                focused.manifest = manifest;
            }
            SettingsField::PrivateAccessMode => {
                if private_access_profile_settings_locked(self.private_access.focused()) {
                    bail!("disconnect Private Access before changing data plane mode");
                }
                if self.private_access.focused().manifest.id == "sonicwall" {
                    if parse_private_access_mode(value)? != PrivateAccessMode::Tun {
                        bail!("SonicWall private access supports TUN mode only");
                    }
                    self.private_access.focused_mut().mode = PrivateAccessMode::Tun;
                } else {
                    self.private_access.focused_mut().mode = parse_private_access_mode(value)?;
                }
            }
            SettingsField::PrivateAccessServer => {
                self.private_access.focused_mut().server = value.to_string();
            }
            SettingsField::PrivateAccessPort => {
                self.private_access.focused_mut().port = parse_positive(value)?
            }
            SettingsField::PrivateAccessUsername => {
                self.private_access.focused_mut().username = value.to_string();
            }
            SettingsField::PrivateAccessPassword => {
                self.private_access.focused_mut().password = value.to_string();
            }
            SettingsField::PrivateAccessPasswordEnv => {
                self.private_access.focused_mut().password_env = value.to_string();
            }
            SettingsField::PrivateAccessBridgeListen => {
                value
                    .parse::<SocketAddrV4>()
                    .context("bridge listen must be an IPv4:PORT address")?;
                self.private_access.focused_mut().bridge_listen = value.to_string();
            }
            SettingsField::PrivateAccessUseInternetProxy => {
                if private_access_profile_settings_locked(self.private_access.focused()) {
                    bail!("disconnect Private Access before changing SonicWall transport");
                }
                if self.private_access.focused().manifest.id != "sonicwall" {
                    bail!("Internet proxy transport is only configurable for SonicWall");
                }
                self.private_access.focused_mut().use_internet_proxy = parse_bool_setting(value)?;
            }
            SettingsField::PrivateAccessTlsVerify => {
                self.private_access.focused_mut().tls_verify = parse_bool_setting(value)?;
            }
        }
        self.save_runtime_state()?;
        self.ensure_auto_pick_background_worker_after_state_change()?;
        self.set_status_only(format!("Saved {}", settings_field_label(field)));
        Ok(())
    }

    fn move_help_next(&mut self) {
        self.help_index = (self.help_index + 1).min(help_binding_count().saturating_sub(1));
    }

    fn move_help_previous(&mut self) {
        self.help_index = self.help_index.saturating_sub(1);
    }

    fn open_latency_chart(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Latency history is available for Internet Proxy nodes only");
            return Ok(());
        }
        let Some(group_name) = self.selected_group().map(|group| group.name.clone()) else {
            self.set_status_only("No selector group available for latency history");
            return Ok(());
        };
        let Some(node) = self.selected_member_name() else {
            self.set_status_only("No node selected for latency history");
            return Ok(());
        };
        let Some(store) = &self.benchmark_store else {
            self.set_status_only("SQLite latency history is unavailable");
            return Ok(());
        };
        let samples = store.node_latency_history(&group_name, &node, 200)?;
        if samples.iter().all(|sample| sample.delay_ms.is_none()) {
            self.set_status_only(format!("No latency history for {}", node));
            return Ok(());
        }
        let count = samples.len();
        self.latency_chart = Some(LatencyChartState {
            selector: group_name,
            node: node.clone(),
            samples,
            window: LATENCY_CHART_DEFAULT_WINDOW,
            threshold_ms: self.auto_select_threshold_ms,
            last_refresh: Instant::now(),
        });
        self.set_status_only(format!("Showing {} latency samples for {}", count, node));
        Ok(())
    }

    fn zoom_latency_chart_in(&mut self) {
        let Some(chart) = self.latency_chart.as_mut() else {
            return;
        };
        chart.window = latency_chart_zoom_in(chart.window);
        let label = latency_chart_window_label(chart.window);
        self.set_status_only(format!("Latency chart window: {label}"));
    }

    fn zoom_latency_chart_out(&mut self) {
        let Some(chart) = self.latency_chart.as_mut() else {
            return;
        };
        chart.window = latency_chart_zoom_out(chart.window);
        let label = latency_chart_window_label(chart.window);
        self.set_status_only(format!("Latency chart window: {label}"));
    }

    fn maybe_refresh_latency_chart(&mut self) -> Result<()> {
        let Some(chart) = self.latency_chart.as_mut() else {
            return Ok(());
        };
        if chart.last_refresh.elapsed() < LATENCY_CHART_REFRESH_INTERVAL {
            return Ok(());
        }
        let Some(store) = &self.benchmark_store else {
            return Ok(());
        };
        chart.samples = store.node_latency_history(&chart.selector, &chart.node, 200)?;
        chart.last_refresh = Instant::now();
        Ok(())
    }

    fn move_next(&mut self) {
        match self.focus {
            Focus::Groups => match self.left_pane_section {
                LeftPaneSection::Internet => {
                    let group_count = self.displayed_group_names().len();
                    if self.displayed_group_index() + 1 < group_count {
                        if self.implicit_root_mode() {
                            self.internet_route_index += 1;
                        } else {
                            self.group_index += 1;
                        }
                        self.sync_member_selection_to_current();
                    } else if self.private_access.is_configured() {
                        self.left_pane_section = LeftPaneSection::Intranet;
                        self.private_access.focused_index = 0;
                        self.intranet_detail_scroll = 0;
                    }
                }
                LeftPaneSection::Intranet => {
                    if self.private_access.focused_index + 1 < self.private_access.profiles.len() {
                        self.private_access.focused_index += 1;
                        self.intranet_detail_scroll = 0;
                    }
                }
            },
            Focus::Members => {
                if self.showing_intranet_details() {
                    let max_scroll = self.intranet_detail_line_count().saturating_sub(1) as u16;
                    self.intranet_detail_scroll = self
                        .intranet_detail_scroll
                        .saturating_add(1)
                        .min(max_scroll);
                    return;
                }
                let members = self.displayed_members();
                if members.is_empty() {
                    return;
                }
                let current_index = self.displayed_member_index().unwrap_or(0);
                if current_index + 1 < members.len() {
                    self.sync_selection_to_member_name(&members[current_index + 1]);
                }
            }
        }
    }

    fn move_previous(&mut self) {
        match self.focus {
            Focus::Groups => match self.left_pane_section {
                LeftPaneSection::Internet => {
                    if self.displayed_group_index() > 0 {
                        if self.implicit_root_mode() {
                            self.internet_route_index -= 1;
                        } else {
                            self.group_index -= 1;
                        }
                        self.sync_member_selection_to_current();
                    }
                }
                LeftPaneSection::Intranet => {
                    if self.private_access.focused_index > 0 {
                        self.private_access.focused_index -= 1;
                        self.intranet_detail_scroll = 0;
                    } else if !self.displayed_group_names().is_empty() {
                        self.left_pane_section = LeftPaneSection::Internet;
                        self.intranet_detail_scroll = 0;
                        if self.implicit_root_mode() {
                            self.internet_route_index =
                                self.displayed_group_names().len().saturating_sub(1);
                        } else {
                            self.group_index = self.groups.len().saturating_sub(1);
                        }
                        self.sync_member_selection_to_current();
                    }
                }
            },
            Focus::Members => {
                if self.showing_intranet_details() {
                    self.intranet_detail_scroll = self.intranet_detail_scroll.saturating_sub(1);
                    return;
                }
                let members = self.displayed_members();
                if members.is_empty() {
                    return;
                }
                let current_index = self.displayed_member_index().unwrap_or(0);
                if current_index > 0 {
                    self.sync_selection_to_member_name(&members[current_index - 1]);
                }
            }
        }
    }

    fn move_first(&mut self) {
        match self.focus {
            Focus::Groups => {
                self.left_pane_section = LeftPaneSection::Internet;
                if self.implicit_root_mode() {
                    self.internet_route_index = 0;
                } else {
                    self.group_index = 0;
                }
                self.sync_member_selection_to_current();
            }
            Focus::Members => {
                if self.showing_intranet_details() {
                    self.intranet_detail_scroll = 0;
                    return;
                }
                if let Some(first) = self.displayed_members().first().cloned() {
                    self.sync_selection_to_member_name(&first);
                }
            }
        }
    }

    fn move_last(&mut self) {
        match self.focus {
            Focus::Groups => {
                if self.private_access.is_configured() {
                    self.left_pane_section = LeftPaneSection::Intranet;
                    self.private_access.focused_index =
                        self.private_access.profiles.len().saturating_sub(1);
                    self.intranet_detail_scroll = 0;
                } else if self.implicit_root_mode() {
                    let groups = self.displayed_group_names();
                    if !groups.is_empty() {
                        self.internet_route_index = groups.len() - 1;
                        self.sync_member_selection_to_current();
                    }
                } else if !self.groups.is_empty() {
                    self.group_index = self.groups.len() - 1;
                    self.sync_member_selection_to_current();
                }
            }
            Focus::Members => {
                if self.showing_intranet_details() {
                    self.intranet_detail_scroll =
                        self.intranet_detail_line_count().saturating_sub(1) as u16;
                    return;
                }
                if let Some(last) = self.displayed_members().last().cloned() {
                    self.sync_selection_to_member_name(&last);
                }
            }
        }
    }

    fn activate_selection(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            let profile_id = self.private_access.focused().id.clone();
            self.focus = Focus::Members;
            self.set_status_only(format!(
                "Showing Intranet Proxy details for {profile_id}; press V to connect or disconnect"
            ));
            return Ok(());
        }
        if self.focus == Focus::Groups {
            if self.implicit_root_mode() {
                self.activate_root_choice()?;
            } else {
                self.focus = Focus::Members;
            }
            return Ok(());
        }

        let Some(group) = self.selected_member_panel_group() else {
            bail!("no selector group available");
        };
        let group_name = group.name.clone();
        if self.implicit_root_mode() && !self.selected_member_panel_is_manual_selector() {
            self.activate_root_choice()?;
            return Ok(());
        }
        let Some(member) = group.members.get(self.member_index).cloned() else {
            bail!("no proxy available in selected group");
        };
        let parent_switch = if self.implicit_root_mode() {
            self.selected_root_choice_name().and_then(|choice| {
                self.implicit_root_group()
                    .map(|root| (root.name.clone(), choice))
            })
        } else {
            None
        };
        self.client
            .switch_proxy(&group_name, &member)
            .with_context(|| format!("failed to switch {} to {}", group_name, member))?;
        if let Some((parent, route_group)) = parent_switch {
            self.client
                .switch_proxy(&parent, &route_group)
                .with_context(|| format!("failed to switch {} to {}", parent, route_group))?;
        }
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        self.set_switch_status(&group_name, &member);
        Ok(())
    }

    fn cycle_clash_mode(&mut self) -> Result<()> {
        let current = self.clash_mode.as_deref();
        let next = next_clash_mode(current, &self.clash_modes);
        self.client
            .set_mode(&next)
            .with_context(|| format!("failed to switch Clash mode to {next}"))?;
        self.clash_mode = Some(next.clone());
        self.set_status_only(format!("Switched Clash mode to {next}"));
        Ok(())
    }

    fn activate_root_choice(&mut self) -> Result<()> {
        let Some(root) = self.implicit_root_group() else {
            bail!("no implicit root selector available");
        };
        let root_name = root.name.clone();
        let Some(choice) = self.selected_root_choice_name() else {
            bail!("no selectable choice available");
        };
        self.client
            .switch_proxy(&root_name, &choice)
            .with_context(|| format!("failed to switch {} to {}", root_name, choice))?;
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        self.set_switch_status(&root_name, &choice);
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        let previous_group_name = self.selected_group().map(|group| group.name.clone());
        let previous_choice_name = self.selected_root_choice_name();
        let config = self.client.fetch_config()?;
        let groups = self.client.fetch_selector_groups()?;
        if groups.is_empty() {
            bail!("no selector groups returned by controller");
        }
        self.clash_mode = config.mode;
        self.clash_modes = config.mode_list;
        self.groups = groups;
        if self.implicit_root_mode() {
            let choices = self.displayed_group_names();
            self.internet_route_index = previous_choice_name
                .as_ref()
                .and_then(|name| choices.iter().position(|choice| choice == name))
                .or_else(|| {
                    self.implicit_root_group()
                        .and_then(|root| root.current.as_deref())
                        .and_then(|current| choices.iter().position(|choice| choice == current))
                })
                .unwrap_or(0);
        } else {
            self.group_index = previous_group_name
                .and_then(|name| self.groups.iter().position(|group| group.name == name))
                .unwrap_or(0);
            self.internet_route_index = 0;
        }
        self.sync_member_selection_to_current();
        self.status = format!("Loaded {} selector groups", self.groups.len());
        Ok(())
    }

    fn sync_member_selection_to_current(&mut self) {
        let next_index =
            self.selected_member_panel_group()
                .and_then(|group| {
                    group.current.as_deref().and_then(|current| {
                        group.members.iter().position(|member| member == current)
                    })
                })
                .unwrap_or(0);
        self.member_index = next_index;
        self.sync_selection_to_displayed_members();
    }

    fn start_group_benchmark(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Latency tests are available for Internet Proxy nodes only");
            return Ok(());
        }
        let Some(group) = self.selected_member_panel_group().cloned() else {
            bail!("no selector group available");
        };
        if self
            .benchmark_jobs
            .iter()
            .any(|job| job.group == group.name)
        {
            self.set_status_only(format!("Latency test already running for {}", group.name));
            return Ok(());
        }
        let candidate_names = self.benchmark_candidates_for_group(&group);
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            nodes: Some(candidate_names.clone()),
        };
        if candidate_names.is_empty() {
            self.set_status_only(format!(
                "No nodes in {} matched filter '{}'",
                group.name, self.benchmark_filter
            ));
            return Ok(());
        }
        self.prepare_group_benchmark(&group.name, candidate_names.clone());
        self.spawn_benchmark_job(
            group.name.clone(),
            candidate_names,
            request,
            BenchmarkJobKind::Group,
        );
        self.set_status_only(format!(
            "Testing latency for {} with filter '{}' in background (max {} concurrent)...",
            group.name, self.benchmark_filter, self.benchmark_max_concurrency
        ));
        Ok(())
    }

    fn start_member_benchmark(&mut self) -> Result<()> {
        if self.showing_intranet_details() {
            self.set_status_only("Latency tests are available for Internet Proxy nodes only");
            return Ok(());
        }
        let Some(group) = self.selected_member_panel_group().cloned() else {
            bail!("no selector group available");
        };
        let displayed_members = self.displayed_members();
        let Some(member) = self
            .displayed_member_index()
            .and_then(|index| displayed_members.get(index))
            .cloned()
        else {
            bail!("no proxy available in selected group");
        };
        if let Some((last_group, last_member, last_started)) = &self.last_single_node_benchmark
            && last_group == &group.name
            && last_member == &member
            && last_started.elapsed() < SINGLE_NODE_RETEST_DEBOUNCE
        {
            self.set_status_only(format!(
                "Ignoring repeated retest for {} / {} (debounced)",
                group.name, member
            ));
            return Ok(());
        }
        if self
            .benchmark_jobs
            .iter()
            .any(|job| job.group == group.name && job.nodes.iter().any(|node| node == &member))
        {
            self.set_status_only(format!(
                "Latency test already running for {} / {}",
                group.name, member
            ));
            return Ok(());
        }
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: 1,
            nodes: Some(vec![member.clone()]),
        };
        self.prepare_node_benchmark(&group.name, &member);
        self.spawn_benchmark_job(
            group.name.clone(),
            vec![member.clone()],
            request,
            BenchmarkJobKind::SingleNode {
                node: member.clone(),
            },
        );
        self.last_single_node_benchmark =
            Some((group.name.clone(), member.clone(), Instant::now()));
        self.set_status_only(format!(
            "Testing latency for {} / {} in background...",
            group.name, member
        ));
        Ok(())
    }

    fn prepare_group_benchmark(&mut self, group: &str, candidates: Vec<String>) {
        let summary = self
            .benchmarks
            .entry(group.to_string())
            .or_insert_with(|| BenchmarkSummary::empty(group.to_string()));
        summary.selector = group.to_string();
        summary.pattern = self.benchmark_filter.clone();
        summary.url = self.benchmark_url.clone();
        summary.timeout_ms = self.benchmark_timeout_ms;
        summary.max_concurrency = self.benchmark_max_concurrency.max(1);
        summary.reset_pending(candidates);
    }

    fn prepare_node_benchmark(&mut self, group: &str, node: &str) {
        let summary = self
            .benchmarks
            .entry(group.to_string())
            .or_insert_with(|| BenchmarkSummary::empty(group.to_string()));
        summary.selector = group.to_string();
        summary.pattern = self.benchmark_filter.clone();
        summary.url = self.benchmark_url.clone();
        summary.timeout_ms = self.benchmark_timeout_ms;
        summary.max_concurrency = 1;
        summary.upsert_pending(node.to_string());
    }

    fn spawn_benchmark_job(
        &mut self,
        group: String,
        nodes: Vec<String>,
        request: BenchmarkRequest,
        kind: BenchmarkJobKind,
    ) {
        let (tx, rx) = mpsc::channel();
        let worker = spawn_benchmark_worker(
            self.client.base_url.clone(),
            self.client.client.clone(),
            request,
            tx,
        );
        self.benchmark_jobs.push(BenchmarkJob {
            group,
            nodes,
            kind,
            receiver: rx,
            worker,
        });
    }

    fn toggle_latency_sort_mode(&mut self) {
        self.latency_sort_mode = !self.latency_sort_mode;
        let status = if self.latency_sort_mode {
            "Sort order: LATENCY ORDER (hide failed-tested nodes, sort successful nodes by delay)"
                .to_string()
        } else {
            "Sort order: SELECTOR ORDER (original selector order with current filter)".to_string()
        };
        self.set_status_only(status);
    }

    fn toggle_auto_select(&mut self) -> Result<()> {
        if self.auto_select_enabled {
            self.auto_select_enabled = false;
            self.auto_select_selector = None;
            self.save_runtime_state()?;
            if self.background_worker_management_enabled() {
                self.stop_live_background_auto_pick_task()?;
            }
            self.set_status_only("Auto-pick disabled; background worker stopped");
            return Ok(());
        }

        let Some(group_name) = self.selected_group().map(|group| group.name.clone()) else {
            self.set_status_only("No selector group available for auto-pick");
            return Ok(());
        };
        self.auto_select_enabled = true;
        self.auto_select_selector = Some(group_name.clone());
        self.last_auto_select_benchmark = None;
        self.save_runtime_state()?;
        if self.background_worker_management_enabled() {
            let worker = self.ensure_auto_pick_background_worker()?;
            self.set_status_only(format!(
                "Auto-pick enabled for {} via background worker pid {} ({}, {}ms threshold, every {}s)",
                group_name,
                worker.pid(),
                self.benchmark_scope_label(),
                self.auto_select_threshold_ms,
                self.auto_select_interval.as_secs()
            ));
        } else {
            self.set_status_only(format!(
                "Auto-pick enabled for {} ({}, {}ms threshold, every {}s)",
                group_name,
                self.benchmark_scope_label(),
                self.auto_select_threshold_ms,
                self.auto_select_interval.as_secs()
            ));
        }
        Ok(())
    }

    fn auto_select_benchmark_due(&self, now: Instant) -> bool {
        self.auto_pick_config()
            .benchmark_due(self.last_auto_select_benchmark, now)
    }

    fn maybe_start_auto_select_benchmark(&mut self) -> Result<()> {
        let now = Instant::now();
        if !self.auto_select_benchmark_due(now) {
            return Ok(());
        }
        let Some(group) = self.auto_select_group().cloned() else {
            return Ok(());
        };
        if self
            .benchmark_jobs
            .iter()
            .any(|job| job.group == group.name)
        {
            return Ok(());
        }

        let candidate_names = self.benchmark_candidates_for_group(&group);
        let request = BenchmarkRequest {
            selector: group.name.clone(),
            pattern: self.benchmark_filter.clone(),
            url: self.benchmark_url.clone(),
            timeout_ms: self.benchmark_timeout_ms,
            request_timeout: self.benchmark_request_timeout,
            max_concurrency: self.benchmark_max_concurrency,
            nodes: Some(candidate_names.clone()),
        };
        self.last_auto_select_benchmark = Some(now);
        if candidate_names.is_empty() {
            self.set_status_only(format!(
                "Auto-pick found no nodes in {} for {}",
                group.name,
                self.benchmark_scope_label()
            ));
            return Ok(());
        }

        self.prepare_group_benchmark(&group.name, candidate_names.clone());
        self.spawn_benchmark_job(
            group.name.clone(),
            candidate_names,
            request,
            BenchmarkJobKind::AutoSelect,
        );
        self.set_status_only(format!(
            "Auto-pick testing latency for {} ({})...",
            group.name,
            self.benchmark_scope_label()
        ));
        Ok(())
    }

    fn benchmark_scope_label(&self) -> String {
        self.auto_pick_config().scope_label()
    }

    fn auto_select_group(&self) -> Option<&ProxyGroup> {
        self.auto_select_selector
            .as_deref()
            .and_then(|selector| self.group_by_name(selector))
            .or_else(|| self.selected_group())
    }

    fn auto_select_switch_plan(
        &self,
        group: &ProxyGroup,
        summary: &BenchmarkSummary,
    ) -> AutoPickDecision {
        self.auto_pick_config().switch_decision(
            group,
            summary,
            self.implicit_root_parent_switch_for_group(&group.name),
        )
    }

    fn finish_auto_select_benchmark(
        &mut self,
        group_name: &str,
        summary: &BenchmarkSummary,
    ) -> Result<()> {
        let Some(group) = self
            .groups
            .iter()
            .find(|group| group.name == group_name)
            .cloned()
        else {
            self.set_status_only(format!(
                "Auto-pick finished for missing group {}",
                group_name
            ));
            return Ok(());
        };

        let plan = self.auto_select_switch_plan(&group, summary);
        if plan.target_node.is_none() && plan.parent_switch.is_none() {
            let current = group.current.as_deref().unwrap_or("unset");
            self.set_status_only(format!(
                "Auto-pick kept {} on {} (threshold {}ms)",
                group_name, current, self.auto_select_threshold_ms
            ));
            return Ok(());
        }

        if let Some(target) = &plan.target_node {
            self.client
                .switch_proxy(group_name, target)
                .with_context(|| {
                    format!("auto-pick failed to switch {} to {}", group_name, target)
                })?;
        }
        if let Some((parent, route_group)) = &plan.parent_switch {
            self.client
                .switch_proxy(parent, route_group)
                .with_context(|| {
                    format!("auto-pick failed to switch {} to {}", parent, route_group)
                })?;
        }
        if REFRESH_DEBOUNCE > Duration::ZERO {
            std::thread::sleep(REFRESH_DEBOUNCE);
        }
        self.refresh()?;
        self.save_runtime_state()?;
        match (&plan.target_node, &plan.parent_switch) {
            (Some(target), Some((_, route_group))) => self.set_status_only(format!(
                "Auto-pick switched {} to {} and selected {}",
                group_name, target, route_group
            )),
            (Some(target), None) => {
                self.set_status_only(format!("Auto-pick switched {} to {}", group_name, target))
            }
            (None, Some((_, route_group))) => {
                let current = group.current.as_deref().unwrap_or("unset");
                self.set_status_only(format!(
                    "Auto-pick selected {}; kept {} on {} (threshold {}ms)",
                    route_group, group_name, current, self.auto_select_threshold_ms
                ));
            }
            (None, None) => {}
        }
        Ok(())
    }

    fn record_benchmark_result(
        &self,
        group: &str,
        filter: &str,
        job_kind: &BenchmarkJobKind,
        result: &crate::controller::BenchmarkResult,
    ) -> Result<()> {
        let Some(store) = &self.benchmark_store else {
            return Ok(());
        };
        store
            .record_benchmark(&BenchmarkRecord {
                selector: group,
                node: &result.name,
                filter,
                delay_ms: result.delay,
                completed: result.completed,
                job_kind: benchmark_job_kind_label(job_kind),
            })
            .with_context(|| format!("failed to record benchmark result for {}", result.name))
            .unwrap_or_else(|error| {
                eprintln!("warning: {error:#}");
            });
        Ok(())
    }

    fn poll_benchmark_updates(&mut self) -> Result<()> {
        let mut finished_indexes = Vec::new();

        for index in 0..self.benchmark_jobs.len() {
            let mut finished = false;
            loop {
                match self.benchmark_jobs[index].receiver.try_recv() {
                    Ok(BenchmarkEvent::Progress(result)) => {
                        let group = self.benchmark_jobs[index].group.clone();
                        let kind = self.benchmark_jobs[index].kind.clone();
                        let mut filter = self.benchmark_filter.clone();
                        if let Some(summary) =
                            self.benchmarks.get_mut(&self.benchmark_jobs[index].group)
                        {
                            filter = summary.pattern.clone();
                            summary.update_result(result.clone());
                            self.status = format!(
                                "Testing latency for {}... best so far: {}",
                                self.benchmark_jobs[index].group,
                                summary.best_label()
                            );
                        }
                        self.record_benchmark_result(&group, &filter, &kind, &result)?;
                    }
                    Ok(BenchmarkEvent::Finished) => {
                        finished = true;
                        let group = self.benchmark_jobs[index].group.clone();
                        let kind = self.benchmark_jobs[index].kind.clone();
                        if let Some(summary) = self.benchmarks.get(&group) {
                            match kind {
                                BenchmarkJobKind::Group => {
                                    if let Some(best) = summary.best_success() {
                                        self.set_status_only(format!(
                                            "Latency tested {}: best is {} ({})",
                                            group,
                                            best.name,
                                            best.display_delay()
                                        ));
                                    } else {
                                        self.set_status_only(format!(
                                            "Latency tested {} but no healthy node matched",
                                            group
                                        ));
                                    }
                                }
                                BenchmarkJobKind::AutoSelect => {
                                    let summary = summary.clone();
                                    self.finish_auto_select_benchmark(&group, &summary)?;
                                }
                                BenchmarkJobKind::SingleNode { node } => {
                                    let result = summary.find_result(&node);
                                    let status = match result {
                                        Some(result) if result.delay.is_some() => format!(
                                            "Latency tested {} / {}: {}",
                                            group,
                                            node,
                                            result.display_delay()
                                        ),
                                        Some(_) => {
                                            format!("Latency tested {} / {}: failed", group, node)
                                        }
                                        None => {
                                            format!(
                                                "Latency test finished for {} / {}",
                                                group, node
                                            )
                                        }
                                    };
                                    self.set_status_only(status);
                                }
                            }
                        }
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        let group = self.benchmark_jobs[index].group.clone();
                        self.set_status_only(format!(
                            "Latency test worker for {} disconnected",
                            group
                        ));
                        break;
                    }
                }
            }
            if finished {
                finished_indexes.push(index);
            }
        }

        for index in finished_indexes.into_iter().rev() {
            let job = self.benchmark_jobs.swap_remove(index);
            let _ = job.worker.join();
        }

        Ok(())
    }

    fn start_verify(&mut self) {
        if self.verify_job.is_some() {
            self.set_status_only("Network verification is already running");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let targets = match parse_verification_targets(&self.verify_targets) {
            Ok(targets) if !targets.is_empty() => targets,
            Ok(_) => {
                self.set_status_only("Configure verification targets in settings first");
                return;
            }
            Err(error) => {
                self.set_status_only(format!("Verification targets invalid: {error}"));
                return;
            }
        };
        let proxy_server = self.system_proxy.resolved_server();
        let worker_proxy_server = proxy_server.clone();
        let worker = thread::spawn(move || {
            let report = run_verification(&worker_proxy_server, &targets);
            let _ = tx.send(report);
        });
        self.verify_job = Some(VerifyJob {
            receiver: rx,
            worker,
        });
        self.set_status_only(format!(
            "Running network verification via {proxy_server}..."
        ));
    }

    fn poll_verify_updates(&mut self) {
        let Some(job) = self.verify_job.as_ref() else {
            return;
        };
        let result = match job.receiver.try_recv() {
            Ok(report) => report,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.verify_job = None;
                self.set_status_with_flash("Network verification failed: worker disconnected");
                return;
            }
        };
        let job = self.verify_job.take().expect("verify job exists");
        let _ = job.worker.join();
        self.set_status_with_flash(result.summary_line());
    }

    fn start_managed_sing_box(&mut self) -> Result<()> {
        let report = self.sing_box.start(&self.client)?;
        if report.replaced_existing() {
            self.status = format!("Restarted managed sing-box {}", report.transition());
        } else {
            self.status = format!("Started managed sing-box {}", report.started_process());
        }
        Ok(())
    }

    fn restart_managed_sing_box(&mut self) -> Result<RestartReceipt> {
        self.sing_box.restart()
    }

    fn shutdown_managed_sing_box(&mut self) -> Result<()> {
        if self.sing_box.is_leaving_running() {
            return Ok(());
        }
        if self.background_worker_management_enabled() {
            self.stop_live_background_auto_pick_task()?;
        }
        self.sing_box.shutdown()
    }

    fn keep_sing_box_running_in_background(&mut self) -> Result<bool> {
        let auto_pick_pid = if self.auto_select_enabled {
            match self.ensure_auto_pick_background_worker() {
                Ok(worker) => Some(worker.pid()),
                Err(error) => {
                    self.set_status_only(format!(
                        "Failed to start background auto-pick: {error:#}"
                    ));
                    return Ok(true);
                }
            }
        } else {
            None
        };
        let private_access_sessions = match self.detach_private_access_for_background() {
            Ok(count) => count,
            Err(error) => {
                self.set_status_only(format!(
                    "Failed to keep Private Access running in background: {error:#}"
                ));
                return Ok(true);
            }
        };
        if let Err(error) = self.save_runtime_state() {
            self.set_status_only(format!(
                "Failed to save background Private Access state: {error:#}"
            ));
            return Ok(true);
        }
        self.sing_box.leave_running();
        let mut parts = vec!["sing-box".to_string()];
        if let Some(pid) = auto_pick_pid {
            parts.push(format!("auto-pick pid {pid}"));
        }
        if private_access_sessions > 0 {
            parts.push(format!(
                "{private_access_sessions} Private Access session(s)"
            ));
        }
        self.set_status_only(format!(
            "Leaving TUI; {} continue in background",
            parts.join(", ")
        ));
        Ok(false)
    }

    fn ensure_auto_pick_background_worker_if_enabled(&mut self) -> Result<()> {
        if !self.auto_select_enabled || !self.background_worker_management_enabled() {
            return Ok(());
        }
        let worker = self.ensure_auto_pick_background_worker()?;
        self.set_status_only(format!(
            "Auto-pick background worker {} pid {}",
            worker.label(),
            worker.pid()
        ));
        Ok(())
    }

    fn ensure_auto_pick_background_worker_after_state_change(&mut self) -> Result<()> {
        if self.auto_select_enabled && self.background_worker_management_enabled() {
            self.ensure_auto_pick_background_worker()?;
        }
        Ok(())
    }

    fn background_worker_management_enabled(&self) -> bool {
        self.state_store.is_some() && !cfg!(test)
    }

    fn ensure_auto_pick_background_worker(&mut self) -> Result<BackgroundWorkerEnsure> {
        let config = self.auto_pick_config();
        let launch = self.background_launch_spec();
        self.background_auto_pick.ensure(&config, &launch)
    }

    fn stop_live_background_auto_pick_task(&mut self) -> Result<()> {
        self.background_auto_pick.stop()
    }

    fn background_status_snapshot(
        &self,
        worker_status: String,
        generation: u64,
    ) -> BackgroundStatusSnapshot {
        BackgroundStatusSnapshot {
            kind: BACKGROUND_TASK_KIND.to_string(),
            pid: std::process::id(),
            controller: self.client.base_url.clone(),
            config_path: self.system_proxy_config_path.clone(),
            max_concurrency: self.benchmark_max_concurrency,
            started_at_unix: self.background_started_at_unix,
            status_generation: generation,
            worker_status,
            updated_at_unix: current_unix_timestamp(),
            auto_pick_enabled: self.auto_select_enabled,
            auto_pick_selector: self.auto_select_selector.clone(),
            filter: self.benchmark_filter.clone(),
            latency: self.background_latency_snapshot(),
        }
    }

    fn background_latency_snapshot(&self) -> Option<BackgroundLatencySnapshot> {
        let group = self.auto_select_group()?;
        let summary = self.benchmarks.get(&group.name)?;
        Some(BackgroundLatencySnapshot {
            selector: summary.selector.clone(),
            current: summary.current.clone(),
            pattern: summary.pattern.clone(),
            url: summary.url.clone(),
            timeout_ms: summary.timeout_ms,
            max_concurrency: summary.max_concurrency,
            results: summary
                .results
                .iter()
                .map(|result| BackgroundLatencyResult {
                    name: result.name.clone(),
                    delay: result.delay,
                    completed: result.completed,
                })
                .collect(),
        })
    }

    fn run_headless_auto_pick_loop(&mut self) -> Result<()> {
        let control = HeadlessWorkerControl::start(HeadlessWorkerMetadata::new(
            self.client.base_url.clone(),
            self.system_proxy_config_path.clone(),
            self.benchmark_max_concurrency,
            self.background_started_at_unix,
        ))?;
        self.auto_select_enabled = false;
        let mut last_published_status = String::new();
        let mut status_generation = 0;
        loop {
            while let Some(request) = control.try_request() {
                match request.command.clone() {
                    HeadlessWorkerCommand::Status => request.respond(
                        self.background_status_snapshot(self.status.clone(), status_generation),
                    ),
                    HeadlessWorkerCommand::ApplyConfig(config) => {
                        self.apply_background_auto_pick_config(config);
                        last_published_status.clear();
                        status_generation = status_generation.saturating_add(1);
                        request.respond(self.background_status_snapshot(
                            "configuration applied".to_string(),
                            status_generation,
                        ));
                    }
                    HeadlessWorkerCommand::Stop => {
                        status_generation = status_generation.saturating_add(1);
                        request.respond(
                            self.background_status_snapshot(
                                "stopping".to_string(),
                                status_generation,
                            ),
                        );
                        control.unregister();
                        return Ok(());
                    }
                }
            }

            if self.auto_select_enabled {
                self.poll_benchmark_updates()?;
                self.maybe_start_auto_select_benchmark()?;
                if self.status != last_published_status
                    && background_status_should_publish(&self.status)
                {
                    status_generation = status_generation.saturating_add(1);
                    last_published_status = self.status.clone();
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn set_system_proxy(&mut self) {
        let bypass_entries = self
            .private_access
            .system_proxy_bypass_entries(&self.bypass_entries);
        match self.system_proxy.toggle(bypass_entries) {
            SystemProxyToggle::AlreadyRunning => {
                self.set_status_only("System proxy update is already running");
            }
            SystemProxyToggle::Started {
                enable: true,
                server,
            } => self.set_status_only(format!("Enabling system proxy at {server}...")),
            SystemProxyToggle::Started { enable: false, .. } => {
                self.set_status_only("Disabling system proxy...");
            }
        }
    }

    fn poll_system_proxy_updates(&mut self) {
        match self.system_proxy.poll() {
            Some(SystemProxyUpdate::Applied(message)) => self.set_status_with_flash(message),
            Some(SystemProxyUpdate::Failed(error)) => self.set_status_with_flash(format!(
                "System proxy update failed: {}",
                truncate_for_width(&error, 90)
            )),
            None => {}
        }
    }

    fn tun_toggle_needs_terminal_prompt(&self) -> bool {
        self.internet_tun.authorization_requirement(&self.sing_box)
            == AuthorizationRequirement::InteractiveSudo
    }

    fn toggle_tun_mode(&mut self) {
        if self.internet_tun.is_transitioning() {
            self.set_status_only("TUN mode update is already running");
            return;
        }
        let state_store = self.state_store.clone();
        let mut runtime_state = self.runtime_state();
        match self.internet_tun.start_toggle(|tun| {
            apply_internet_tun_persistence(&mut runtime_state, tun);
            if let Some(store) = &state_store {
                store.save(&runtime_state)?;
            }
            Ok(())
        }) {
            Ok(InternetTunTarget::Enabled) => self.set_status_only("Enabling TUN mode..."),
            Ok(InternetTunTarget::Disabled) => self.set_status_only("Disabling TUN mode..."),
            Err(error) => self.set_status_with_flash(format!(
                "TUN mode update failed: {}",
                truncate_for_width(&format!("{error:#}"), 90)
            )),
        }
    }

    fn poll_tun_toggle_updates(&mut self) {
        if !self.internet_tun.is_transitioning() {
            return;
        }
        let state_store = self.state_store.clone();
        let mut runtime_state = self.runtime_state();
        let Some(outcome) = self
            .internet_tun
            .poll(&mut self.sing_box, &self.client, |tun| {
                apply_internet_tun_persistence(&mut runtime_state, tun);
                if let Some(store) = &state_store {
                    store.save(&runtime_state)?;
                }
                Ok(())
            })
        else {
            return;
        };
        match outcome {
            InternetTunToggleOutcome::Failed {
                error,
                recovery_warning,
            } => {
                let recovery_note = recovery_warning.map(|warning| {
                    format!(
                        "; failed to clear transition journal: {}",
                        truncate_for_width(&warning, 40)
                    )
                });
                self.set_status_with_flash(format!(
                    "TUN mode update failed: {}{}",
                    truncate_for_width(&error, 90),
                    recovery_note.as_deref().unwrap_or("")
                ));
            }
            InternetTunToggleOutcome::Applied {
                target,
                config_changed,
                restart,
                persistence_warning,
            } => {
                let target_label = if target.is_enabled() {
                    "enabled"
                } else {
                    "disabled"
                };
                let state = if config_changed {
                    target_label.to_string()
                } else {
                    format!("already {target_label}")
                };
                let persist_note = persistence_warning.map(|warning| {
                    format!(
                        "; recovery journal retained: {}",
                        truncate_for_width(&warning, 40)
                    )
                });
                match restart {
                    Ok(restart) => {
                        let restarted =
                            format!("sing-box restarted {}", restart.report.transition());
                        if let Some(error) = restart.controller_error {
                            self.set_status_with_flash(format!(
                                "TUN mode {state}; {restarted}; controller not ready: {}{}",
                                truncate_for_width(&error, 60),
                                persist_note.as_deref().unwrap_or("")
                            ));
                        } else {
                            self.set_status_with_flash(format!(
                                "TUN mode {state}; {restarted}{}",
                                persist_note.as_deref().unwrap_or("")
                            ));
                        }
                    }
                    Err(error) => self.set_status_with_flash(format!(
                        "TUN mode {state} but sing-box restart failed: {error}{}",
                        persist_note.as_deref().unwrap_or("")
                    )),
                }
            }
        }
    }

    fn open_benchmark_filter_modal(&mut self) {
        self.filter_input = Some(self.benchmark_filter.clone());
        self.flash = None;
    }

    fn open_bypass_modal(&mut self) {
        self.bypass_input = Some(self.bypass_entries.join(","));
        self.flash = None;
    }

    fn handle_filter_input_key(&mut self, code: KeyCode) -> Result<bool> {
        let Some(buffer) = self.filter_input.as_mut() else {
            return Ok(true);
        };

        match code {
            KeyCode::Esc | KeyCode::Char(' ') => {
                self.filter_input = None;
                self.set_status_only("Latency filter edit canceled");
            }
            KeyCode::Enter => {
                let value = buffer.trim().to_string();
                self.filter_input = None;
                self.apply_benchmark_filter(value)?;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(ch) => {
                buffer.push(ch);
            }
            _ => {}
        }
        Ok(true)
    }

    fn handle_bypass_input_key(&mut self, code: KeyCode) -> Result<bool> {
        let Some(buffer) = self.bypass_input.as_mut() else {
            return Ok(true);
        };

        match code {
            KeyCode::Esc | KeyCode::Char(' ') => {
                self.bypass_input = None;
                self.set_status_only("Bypass edit canceled");
            }
            KeyCode::Enter => {
                let value = buffer.clone();
                self.bypass_input = None;
                self.apply_bypass_entries(parse_bypass_entries(&value))?;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(ch) => {
                buffer.push(ch);
            }
            _ => {}
        }
        Ok(true)
    }

    fn apply_benchmark_filter(&mut self, value: String) -> Result<()> {
        self.benchmark_filter = value;
        self.sync_selection_to_displayed_members();
        self.last_auto_select_benchmark = None;
        if self.benchmark_filter.is_empty() {
            self.set_status_only("Latency filter cleared");
        } else {
            self.set_status_only(format!("Latency filter set to '{}'", self.benchmark_filter));
        }
        self.save_runtime_state()?;
        self.ensure_auto_pick_background_worker_after_state_change()?;
        Ok(())
    }

    fn apply_bypass_entries(&mut self, entries: Vec<String>) -> Result<()> {
        self.bypass_entries = entries;
        self.save_runtime_state()?;
        self.save_bypass_rule_set()?;
        if self.bypass_entries.is_empty() {
            self.set_status_only("Bypass list cleared");
        } else {
            self.set_status_only(format!(
                "Bypass list saved ({} entries)",
                self.bypass_entries.len()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use super::process_alive_via_ps;
    use super::{
        AUTO_SELECT_THRESHOLD_MS, App, AutoPickDecision, BackgroundLatencyResult,
        BackgroundLatencySnapshot, CONNECTION_REFRESH_INTERVAL, DIRECT_CLASH_MODE, Focus,
        GLOBAL_CLASH_MODE, IntranetDetailSection, LATENCY_CHART_DEFAULT_WINDOW,
        LATENCY_CHART_REFRESH_INTERVAL, LatencyChartState, LeftPaneSection, PrivateAccessMode,
        PrivateAccessProfileRuntime, PrivateAccessRuntime, PrivateAccessState, RULE_CLASH_MODE,
        SettingsEditState, SettingsField, SystemProxy, connection_is_direct,
        is_private_access_settings_field, next_clash_mode, normalize_http_connect_proxy,
        private_access_auth_display_value, private_access_auth_initial_value,
        settings_field_display_value, settings_field_value, sonicwall_http_connect_settings,
        truncate_for_width, visible_settings_fields,
    };
    use crate::controller::{
        ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest,
        BenchmarkResult, BenchmarkSummary, ConnectionInfo, ConnectionMetadata, ConnectionsSnapshot,
        ProxyGroup,
    };
    use crate::defaults::{DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONTROLLER};
    use crate::internet_tun::{InternetTunTransaction, PersistedInternetTun};
    use crate::managed_sing_box::ManagedSingBox;
    use crate::private_access::{PrivateAccessAuthField, PrivateAccessRoute};
    use crate::tui_state::{PrivateAccessProfileState, TuiRuntimeState, TuiStateStore};
    use crossterm::event::KeyCode;
    use crossterm::event::MouseEventKind;
    use reqwest::Client as AsyncClient;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn sonicwall_http_connect_proxy_uses_tui_mixed_inbound() {
        assert_eq!(
            normalize_http_connect_proxy("127.0.0.1:6780").as_deref(),
            Some("127.0.0.1:6780")
        );
        assert_eq!(
            normalize_http_connect_proxy("http://127.0.0.1:6780/").as_deref(),
            Some("127.0.0.1:6780")
        );
        assert_eq!(normalize_http_connect_proxy("  "), None);
    }

    #[test]
    fn sonicwall_transport_setting_is_exclusive() {
        let direct = sonicwall_http_connect_settings(
            false,
            "127.0.0.1:6780",
            Some("manual -> node-a".to_string()),
            "http://127.0.0.1:9992",
            Some("manual".to_string()),
        );
        assert_eq!(direct, (None, None, None, None));

        let proxied = sonicwall_http_connect_settings(
            true,
            "127.0.0.1:6780",
            Some("manual -> node-a".to_string()),
            "http://127.0.0.1:9992",
            Some("manual".to_string()),
        );
        assert_eq!(
            proxied,
            (
                Some("127.0.0.1:6780".to_string()),
                Some("manual -> node-a".to_string()),
                Some("http://127.0.0.1:9992".to_string()),
                Some("manual".to_string()),
            )
        );
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::runtime::Builder as TokioRuntimeBuilder;

    use crate::storage::{BenchmarkRecord, BenchmarkStore, NodeLatencySample};

    static TEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_test_suffix() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let counter = TEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{nanos}-{counter}")
    }

    fn test_db_path() -> std::path::PathBuf {
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

    fn test_bypass_rule_set_path() -> std::path::PathBuf {
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

        App {
            client: ApiClient {
                base_url: DEFAULT_CONTROLLER.to_string(),
                runtime,
                client,
            },
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
            benchmarks: BTreeMap::new(),
            benchmark_jobs: Vec::new(),
            latency_sort_mode: false,
            last_single_node_benchmark: None,
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
            benchmark_store: None,
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
            system_proxy_config_path: PathBuf::from("config.json"),
            system_proxy: SystemProxy::for_test(
                PathBuf::from("config.json"),
                "127.0.0.1:6780",
                false,
            ),
            internet_tun: InternetTunTransaction::new(
                PathBuf::from("config.json"),
                PersistedInternetTun::default(),
            )
            .expect("Internet TUN transaction initializes"),
            china_ip_routing_enabled: false,
            china_ip_routing_explicit: false,
            verify_job: None,
            sing_box: ManagedSingBox::new(
                PathBuf::from("sing-box"),
                PathBuf::from("config.json"),
                false,
            ),
            private_access: PrivateAccessRuntime::with_default_hillstone()
                .expect("private access runtime"),
            private_access_progress: None,
            private_access_auth: None,
        }
    }

    #[test]
    fn tun_toggle_is_documented_in_help_and_status_bar() {
        let mut app = test_app();
        let snapshot = app.presentation_snapshot();
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

        let snapshot = app.presentation_snapshot();
        let context = &snapshot.status.selection_context;
        assert!(context.contains("filter='-香港,-广告'"));
        assert!(context.contains("clash="));
        assert!(context.contains("Pick=Manual"));
        assert!(!context.contains("tested="));
        assert!(!context.contains("auto="));
        app.auto_select_enabled = true;

        let snapshot = app.presentation_snapshot();
        let context = &snapshot.status.selection_context;
        assert!(context.contains("Pick=Auto"));
    }

    #[test]
    fn status_header_keeps_empty_filter_label_in_intranet_details() {
        let mut app = test_app();
        app.benchmark_filter.clear();
        app.left_pane_section = LeftPaneSection::Intranet;

        let snapshot = app.presentation_snapshot();
        assert!(snapshot.status.selection_context.contains("filter=''"));
        assert!(
            snapshot
                .status
                .selection_context
                .contains("Intranet details are shown in the right panel")
        );
    }

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
        app.internet_tun =
            InternetTunTransaction::new(path.clone(), PersistedInternetTun::default())
                .expect("Internet TUN transaction initializes");
        app.handle_key(KeyCode::Char('\\'))
            .expect("backslash is handled");

        assert!(app.internet_tun.is_transitioning());
        assert_eq!(app.status, "Enabling TUN mode...");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&path).is_ok_and(|text| text.contains("\"tun\"")) {
            assert!(Instant::now() < deadline, "config mutation timed out");
            std::thread::yield_now();
        }
        let _ = std::fs::remove_file(path);
    }

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

    fn internet_routes_app() -> App {
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

    #[test]
    fn intranet_detail_navigation_scrolls() {
        let mut app = test_app();
        app.private_access.focused_mut().routes = vec![PrivateAccessRoute {
            cidr: "10.20.0.0/16".to_string(),
        }];
        app.focus = Focus::Members;
        app.left_pane_section = LeftPaneSection::Intranet;

        app.move_next();
        assert_eq!(app.intranet_detail_scroll, 1);
        app.move_previous();
        assert_eq!(app.intranet_detail_scroll, 0);
    }

    #[test]
    fn large_intranet_sections_fold_and_toggle_with_enter() {
        let mut app = test_app();
        app.private_access.focused_mut().routes = (0..103)
            .map(|index| PrivateAccessRoute {
                cidr: format!("10.20.{index}.0/24"),
            })
            .collect();
        app.focus = Focus::Members;
        app.left_pane_section = LeftPaneSection::Intranet;

        let route_range = app
            .intranet_detail_view(app.private_access.focused())
            .sections
            .iter()
            .find(|range| range.section == IntranetDetailSection::Routes)
            .copied()
            .expect("routes section");
        assert!(route_range.foldable);

        app.intranet_detail_scroll = route_range.start as u16;
        app.handle_key(KeyCode::Enter).expect("expand routes");
        let section_key = App::intranet_detail_section_key(
            &app.private_access.focused().id,
            IntranetDetailSection::Routes,
        );
        assert!(app.expanded_intranet_sections.contains(&section_key));

        app.handle_key(KeyCode::Enter).expect("fold routes");
        assert!(!app.expanded_intranet_sections.contains(&section_key));
    }

    #[test]
    fn left_pane_navigation_crosses_between_internet_and_intranet_sections() {
        let mut app = test_app();
        app.private_access
            .profiles
            .push(PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile"));
        app.focus = Focus::Groups;
        app.left_pane_section = LeftPaneSection::Internet;

        app.move_next();
        assert_eq!(app.left_pane_section, LeftPaneSection::Intranet);
        assert_eq!(app.private_access.focused_index, 0);

        app.move_next();
        assert_eq!(app.private_access.focused_index, 1);

        app.move_previous();
        assert_eq!(app.private_access.focused_index, 0);
        app.move_previous();
        assert_eq!(app.left_pane_section, LeftPaneSection::Internet);
        assert_eq!(app.displayed_group_index(), 0);
    }

    #[test]
    fn truncates_wide_strings_without_panicking() {
        let truncated = truncate_for_width("手动选择-自动选择-节点A", 8);
        assert!(truncated.ends_with('…'));
        assert!(!truncated.is_empty());
    }

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

    fn test_connection(host: &str, chains: Vec<&str>) -> ConnectionInfo {
        ConnectionInfo {
            id: "conn-1".to_string(),
            download: 0,
            upload: 0,
            start: None,
            chains: chains.into_iter().map(ToString::to_string).collect(),
            rule: Some("clash_mode=规则 => route(手动选择)".to_string()),
            rule_payload: None,
            metadata: ConnectionMetadata {
                network: Some("tcp".to_string()),
                kind: Some("tun/tun-in".to_string()),
                source_ip: Some("172.19.0.1".to_string()),
                destination_ip: Some("1.1.1.1".to_string()),
                host: Some(host.to_string()),
                destination_port: Some("443".to_string()),
                source_port: None,
                process_path: None,
            },
        }
    }

    #[test]
    fn formats_connection_summary_counts_proxy_and_direct() {
        let mut app = test_app();
        app.connections = ConnectionsSnapshot {
            upload_total: Some(1536),
            download_total: Some(2 * 1024 * 1024),
            memory: None,
            connections: vec![
                test_connection("www.google.com", vec!["node-a", "airtcp", "手动选择"]),
                test_connection("example.cn", vec!["国内直连"]),
            ],
        };

        assert_eq!(
            app.connections_summary_line(),
            "connections active=2 proxy=1 direct=1 up=1.5KiB down=2.0MiB"
        );
    }

    #[test]
    fn connection_helpers_classify_active_rows() {
        let direct = test_connection("example.cn", vec!["国内直连"]);
        let proxied = test_connection("www.google.com", vec!["node-a", "airtcp", "手动选择"]);

        assert!(connection_is_direct(&direct));
        assert!(!connection_is_direct(&proxied));
    }

    #[test]
    fn pressing_c_opens_connection_details() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('c'))
            .expect("open connection panel");

        assert!(app.show_connections);
        assert_eq!(app.status, "Showing active connections");
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
    fn help_panel_moves_selection_with_mouse_wheel() {
        let mut app = test_app();
        app.handle_key(KeyCode::Char('?')).expect("open help");

        app.handle_mouse(MouseEventKind::ScrollDown);
        assert_eq!(app.help_index, 1);

        app.handle_mouse(MouseEventKind::ScrollUp);
        assert_eq!(app.help_index, 0);
    }

    #[test]
    fn pressing_u_without_subscription_state_updates_status() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('u'))
            .expect("manual subscription refresh handled");

        assert_eq!(
            app.status,
            "Subscription refresh is disabled or .suburl was not found"
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
    fn stale_private_access_background_pid_is_discarded_from_state() {
        let mut app = test_app();
        let stale_pid = u32::MAX;
        assert!(!super::process_exists(stale_pid));
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
        assert!(super::process_exists(unrelated_pid));
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
    fn next_clash_mode_cycles_controller_mode_list() {
        let modes = vec![
            GLOBAL_CLASH_MODE.to_string(),
            DIRECT_CLASH_MODE.to_string(),
            RULE_CLASH_MODE.to_string(),
        ];

        assert_eq!(
            next_clash_mode(Some(DIRECT_CLASH_MODE), &modes),
            RULE_CLASH_MODE
        );
        assert_eq!(
            next_clash_mode(Some(RULE_CLASH_MODE), &modes),
            GLOBAL_CLASH_MODE
        );
        assert_eq!(
            next_clash_mode(Some(GLOBAL_CLASH_MODE), &modes),
            DIRECT_CLASH_MODE
        );
    }

    #[test]
    fn next_clash_mode_defaults_to_rule_after_direct() {
        assert_eq!(
            next_clash_mode(Some(DIRECT_CLASH_MODE), &[]),
            RULE_CLASH_MODE
        );
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
        assert!(
            !visible_settings_fields(&app).contains(&SettingsField::PrivateAccessUseInternetProxy)
        );

        app.private_access
            .profiles
            .push(PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile"));
        app.private_access.focused_index = 1;
        assert!(
            visible_settings_fields(&app).contains(&SettingsField::PrivateAccessUseInternetProxy)
        );

        app.apply_settings_value(
            SettingsField::PrivateAccessUseInternetProxy,
            "true".to_string(),
        )
        .expect("proxy choice saves");
        assert!(app.private_access.focused().use_internet_proxy);
        assert!(app.runtime_state().private_access_profiles[1].use_internet_proxy);
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

    #[test]
    fn bypass_modal_updates_state_and_rule_set_file() {
        let path = test_state_path();
        let rule_set_path = test_bypass_rule_set_path();
        let mut app = test_app();
        app.state_store = Some(TuiStateStore::new(&path));
        app.bypass_rule_set_store = Some(crate::tui_state::BypassRuleSetStore::new(&rule_set_path));

        app.handle_key(KeyCode::Char('b'))
            .expect("open bypass modal");
        for ch in "example.com,10.0.0.0/8".chars() {
            app.handle_key(KeyCode::Char(ch)).expect("type bypass");
        }
        app.handle_key(KeyCode::Enter).expect("save bypass");

        let state = TuiStateStore::new(&path).load().expect("load state");
        assert_eq!(
            state.bypass_entries,
            vec!["example.com".to_string(), "10.0.0.0/8".to_string()]
        );
        let rule_set: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&rule_set_path).expect("read rule set"))
                .expect("parse rule set");
        assert_eq!(
            rule_set["rules"][0]["domain_suffix"],
            serde_json::json!(["example.com"])
        );
        assert_eq!(
            rule_set["rules"][1]["ip_cidr"],
            serde_json::json!(["10.0.0.0/8"])
        );

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(rule_set_path);
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
    fn uppercase_b_keeps_managed_sing_box_running_and_exits_tui() {
        let mut app = test_app();

        let keep_running = app.handle_key(KeyCode::Char('B')).expect("handle key");

        assert!(!keep_running);
        assert!(app.sing_box.is_leaving_running());
    }

    #[test]
    fn single_node_benchmark_finish_does_not_flash() {
        let mut app = test_app();
        app.benchmarks.insert(
            "select".to_string(),
            BenchmarkSummary {
                selector: "select".to_string(),
                current: Some("node-a".to_string()),
                pattern: "美国".to_string(),
                url: "https://www.gstatic.com/generate_204".to_string(),
                timeout_ms: 5000,
                max_concurrency: 1,
                results: vec![BenchmarkResult {
                    name: "node-a".to_string(),
                    delay: Some(42),
                    completed: true,
                }],
            },
        );

        let (tx, rx) = mpsc::channel();
        tx.send(BenchmarkEvent::Finished)
            .expect("send finish event");
        let worker = thread::spawn(|| {});
        app.benchmark_jobs.push(BenchmarkJob {
            group: "select".to_string(),
            nodes: vec!["node-a".to_string()],
            kind: BenchmarkJobKind::SingleNode {
                node: "node-a".to_string(),
            },
            receiver: rx,
            worker,
        });

        app.poll_benchmark_updates().expect("poll succeeds");

        assert_eq!(app.status, "Latency tested select / node-a: 42ms");
        assert!(app.flash.is_none());
        assert!(app.benchmark_jobs.is_empty());
    }

    #[test]
    fn toggling_latency_sort_mode_does_not_flash() {
        let mut app = test_app();
        app.set_status_with_flash("existing flash");

        app.toggle_latency_sort_mode();

        assert!(app.latency_sort_mode);
        assert_eq!(
            app.status,
            "Sort order: LATENCY ORDER (hide failed-tested nodes, sort successful nodes by delay)"
        );
        assert!(app.flash.is_none());
    }

    #[test]
    fn group_benchmark_finish_does_not_flash() {
        let mut app = test_app();
        app.benchmarks.insert(
            "select".to_string(),
            BenchmarkSummary {
                selector: "select".to_string(),
                current: Some("node-a".to_string()),
                pattern: "美国".to_string(),
                url: "https://www.gstatic.com/generate_204".to_string(),
                timeout_ms: 5000,
                max_concurrency: 4,
                results: vec![
                    BenchmarkResult {
                        name: "node-a".to_string(),
                        delay: Some(42),
                        completed: true,
                    },
                    BenchmarkResult {
                        name: "node-b".to_string(),
                        delay: Some(80),
                        completed: true,
                    },
                ],
            },
        );

        let (tx, rx) = mpsc::channel();
        tx.send(BenchmarkEvent::Finished)
            .expect("send finish event");
        let worker = thread::spawn(|| {});
        app.benchmark_jobs.push(BenchmarkJob {
            group: "select".to_string(),
            nodes: vec!["node-a".to_string(), "node-b".to_string()],
            kind: BenchmarkJobKind::Group,
            receiver: rx,
            worker,
        });

        app.poll_benchmark_updates().expect("poll succeeds");

        assert_eq!(app.status, "Latency tested select: best is node-a (42ms)");
        assert!(app.flash.is_none());
        assert!(app.benchmark_jobs.is_empty());
    }

    #[test]
    fn benchmark_progress_is_recorded_to_sqlite() {
        let path = test_db_path();
        let mut app = test_app();
        app.benchmark_store = Some(BenchmarkStore::open(&path).expect("open benchmark store"));

        let (tx, rx) = mpsc::channel();
        tx.send(BenchmarkEvent::Progress(BenchmarkResult {
            name: "美国-a".to_string(),
            delay: Some(88),
            completed: true,
        }))
        .expect("send progress event");
        let worker = thread::spawn(|| {});
        app.benchmark_jobs.push(BenchmarkJob {
            group: "select".to_string(),
            nodes: vec!["美国-a".to_string()],
            kind: BenchmarkJobKind::AutoSelect,
            receiver: rx,
            worker,
        });

        app.poll_benchmark_updates().expect("poll succeeds");

        let store = BenchmarkStore::open(&path).expect("reopen benchmark store");
        let rows = store.recent_benchmarks(10).expect("read rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].selector, "select");
        assert_eq!(rows[0].node, "美国-a");
        assert_eq!(rows[0].filter, "美国");
        assert_eq!(rows[0].delay_ms, Some(88));
        assert!(rows[0].completed);
        assert_eq!(rows[0].job_kind, "auto");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pressing_i_opens_latency_chart_for_selected_node() {
        let path = test_db_path();
        let mut app = test_app();
        app.groups[0].members = vec!["node-a".to_string(), "node-b".to_string()];
        app.member_index = 1;
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        store
            .record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "node-b",
                filter: "美国",
                delay_ms: Some(93),
                completed: true,
                job_kind: "single",
            })
            .expect("record benchmark");
        app.benchmark_store = Some(store);

        app.handle_key(KeyCode::Char('i')).expect("open chart");

        let chart = app.latency_chart.as_ref().expect("latency chart");
        assert_eq!(chart.selector, "select");
        assert_eq!(chart.node, "node-b");
        assert_eq!(chart.samples.len(), 1);
        assert_eq!(chart.samples[0].delay_ms, Some(93));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn z_and_shift_z_zoom_latency_chart() {
        let mut app = test_app();
        app.latency_chart = Some(LatencyChartState {
            selector: "select".to_string(),
            node: "node-a".to_string(),
            samples: vec![NodeLatencySample {
                recorded_at_ms: 1_000,
                delay_ms: Some(90),
            }],
            window: LATENCY_CHART_DEFAULT_WINDOW,
            threshold_ms: AUTO_SELECT_THRESHOLD_MS,
            last_refresh: Instant::now(),
        });

        app.handle_key(KeyCode::Char('z')).expect("zoom in");
        assert_eq!(
            app.latency_chart.as_ref().expect("chart").window,
            Duration::from_secs(30 * 60)
        );

        app.handle_key(KeyCode::Char('Z')).expect("zoom out");
        assert_eq!(
            app.latency_chart.as_ref().expect("chart").window,
            LATENCY_CHART_DEFAULT_WINDOW
        );
    }

    #[test]
    fn latency_chart_refreshes_from_sqlite() {
        let path = test_db_path();
        let mut app = test_app();
        let store = BenchmarkStore::open(&path).expect("open benchmark store");
        store
            .record_benchmark(&BenchmarkRecord {
                selector: "select",
                node: "node-a",
                filter: "美国",
                delay_ms: Some(77),
                completed: true,
                job_kind: "auto",
            })
            .expect("record benchmark");
        app.benchmark_store = Some(store);
        app.latency_chart = Some(LatencyChartState {
            selector: "select".to_string(),
            node: "node-a".to_string(),
            samples: Vec::new(),
            window: LATENCY_CHART_DEFAULT_WINDOW,
            threshold_ms: AUTO_SELECT_THRESHOLD_MS,
            last_refresh: Instant::now() - LATENCY_CHART_REFRESH_INTERVAL,
        });

        app.maybe_refresh_latency_chart().expect("refresh chart");

        let chart = app.latency_chart.as_ref().expect("chart");
        assert_eq!(chart.samples.len(), 1);
        assert_eq!(chart.samples[0].delay_ms, Some(77));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pressing_i_without_history_updates_status() {
        let path = test_db_path();
        let mut app = test_app();
        app.benchmark_store = Some(BenchmarkStore::open(&path).expect("open benchmark store"));

        app.handle_key(KeyCode::Char('i')).expect("open chart");

        assert!(app.latency_chart.is_none());
        assert_eq!(app.status, "No latency history for node-a");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn slash_opens_filter_modal_with_current_value() {
        let mut app = test_app();
        app.benchmark_filter = "hk".to_string();

        app.handle_key(KeyCode::Char('/')).expect("open modal");

        assert_eq!(app.filter_input.as_deref(), Some("hk"));
    }

    #[test]
    fn filter_modal_submit_updates_filter() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Char('u')).expect("type");
        app.handle_key(KeyCode::Char('s')).expect("type");
        app.handle_key(KeyCode::Enter).expect("submit");

        assert_eq!(app.benchmark_filter, "美国us");
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Latency filter set to '美国us'");
        assert!(app.flash.is_none());
    }

    #[test]
    fn filter_modal_empty_submit_clears_filter() {
        let mut app = test_app();
        app.auto_select_enabled = true;

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Backspace).expect("backspace");
        app.handle_key(KeyCode::Backspace).expect("backspace");
        app.handle_key(KeyCode::Enter).expect("submit");

        assert!(app.benchmark_filter.is_empty());
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Latency filter cleared");
        assert!(app.auto_select_enabled);
        assert!(app.flash.is_none());
    }

    #[test]
    fn filter_modal_escape_cancels_without_changing_filter() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Char('x')).expect("type");
        app.handle_key(KeyCode::Esc).expect("cancel");

        assert_eq!(app.benchmark_filter, "美国");
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Latency filter edit canceled");
    }

    #[test]
    fn filter_modal_space_cancels_without_changing_filter() {
        let mut app = test_app();

        app.handle_key(KeyCode::Char('/')).expect("open modal");
        app.handle_key(KeyCode::Char('x')).expect("type");
        app.handle_key(KeyCode::Char(' ')).expect("cancel");

        assert_eq!(app.benchmark_filter, "美国");
        assert_eq!(app.filter_input, None);
        assert_eq!(app.status, "Latency filter edit canceled");
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
    fn displayed_members_follow_active_filter() {
        let mut app = test_app();
        app.groups[0].members = vec!["hk-1".to_string(), "us-1".to_string(), "hk-2".to_string()];

        app.apply_benchmark_filter("hk".to_string())
            .expect("apply filter");

        assert_eq!(
            app.displayed_members(),
            vec!["hk-1".to_string(), "hk-2".to_string()]
        );
    }

    #[test]
    fn implicit_root_mode_displays_root_choices_as_left_column() {
        let mut app = internet_routes_app();

        assert!(app.implicit_root_mode());
        assert_eq!(
            app.displayed_group_names(),
            vec!["AirTCP".to_string(), "宝贝云".to_string()]
        );

        app.groups[0].members = vec!["宝贝云".to_string()];

        assert!(app.implicit_root_mode());
        assert_eq!(app.displayed_group_names(), vec!["宝贝云".to_string()]);
    }

    #[test]
    fn implicit_root_mode_supports_single_internet_route_selector() {
        let mut app = internet_routes_app();
        app.groups = vec![
            ProxyGroup {
                name: "手动选择".to_string(),
                kind: "Selector".to_string(),
                current: Some("airtcp".to_string()),
                members: vec!["airtcp".to_string()],
            },
            ProxyGroup {
                name: "airtcp".to_string(),
                kind: "Selector".to_string(),
                current: Some("香港-a".to_string()),
                members: vec!["香港-a".to_string(), "美国-b".to_string()],
            },
        ];
        app.internet_route_index = 0;
        app.member_index = 0;

        assert!(app.implicit_root_mode());

        app.apply_benchmark_filter("美国".to_string())
            .expect("apply filter");

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("airtcp"));
        assert_eq!(app.displayed_members(), vec!["美国-b".to_string()]);
        assert_eq!(
            app.selected_group().map(|group| group.name.as_str()),
            Some("airtcp")
        );
    }

    #[test]
    fn implicit_root_members_follow_selected_choice() {
        let mut app = internet_routes_app();

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("宝贝云"));
        assert_eq!(
            app.displayed_members(),
            vec!["bby-1".to_string(), "bby-2".to_string()]
        );

        app.internet_route_index = 0;
        app.sync_member_selection_to_current();

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("AirTCP"));
        assert_eq!(
            app.displayed_members(),
            vec!["air-1".to_string(), "air-2".to_string()]
        );
        assert_eq!(app.member_index, 0);
    }

    #[test]
    fn implicit_root_excludes_urltest_auto_choice() {
        let mut app = internet_routes_app();
        app.internet_route_index = 0;
        app.sync_member_selection_to_current();

        assert_eq!(app.selected_root_choice_name().as_deref(), Some("AirTCP"));
        assert_eq!(
            app.displayed_members(),
            vec!["air-1".to_string(), "air-2".to_string()]
        );
        assert!(app.selected_member_panel_is_manual_selector());
    }

    #[test]
    fn implicit_root_parent_switch_targets_non_current_internet_route() {
        let app = internet_routes_app();

        assert_eq!(
            app.implicit_root_parent_switch_for_group("AirTCP"),
            Some(("手动选择".to_string(), "AirTCP".to_string()))
        );
        assert_eq!(app.implicit_root_parent_switch_for_group("宝贝云"), None);
        assert_eq!(app.implicit_root_parent_switch_for_group("自动选择"), None);
        assert_eq!(app.implicit_root_parent_switch_for_group("missing"), None);
    }

    #[test]
    fn implicit_root_benchmark_summary_is_scoped_to_selected_choice() {
        let mut app = internet_routes_app();
        app.benchmarks.insert(
            "宝贝云".to_string(),
            BenchmarkSummary {
                selector: "宝贝云".to_string(),
                current: Some("bby-2".to_string()),
                pattern: String::new(),
                url: "https://www.gstatic.com/generate_204".to_string(),
                timeout_ms: 5000,
                max_concurrency: 4,
                results: vec![BenchmarkResult {
                    name: "bby-1".to_string(),
                    delay: Some(88),
                    completed: true,
                }],
            },
        );

        assert_eq!(
            app.selected_benchmark()
                .map(|summary| summary.selector.as_str()),
            Some("宝贝云")
        );
        assert!(
            app.selected_benchmark()
                .and_then(|summary| summary.find_result("bby-1"))
                .is_some()
        );
    }

    #[test]
    fn background_latency_snapshot_updates_visible_benchmark_results() {
        let mut app = test_app();

        app.apply_background_latency_snapshot(Some(&BackgroundLatencySnapshot {
            selector: "select".to_string(),
            current: Some("node-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![
                BackgroundLatencyResult {
                    name: "node-a".to_string(),
                    delay: Some(88),
                    completed: true,
                },
                BackgroundLatencyResult {
                    name: "node-b".to_string(),
                    delay: None,
                    completed: true,
                },
            ],
        }));

        let summary = app.selected_benchmark().expect("benchmark summary");
        assert_eq!(
            summary.find_result("node-a").map(|result| result.delay),
            Some(Some(88))
        );
        assert_eq!(
            summary
                .find_result("node-b")
                .map(|result| result.display_delay()),
            Some("fail".to_string())
        );
    }

    #[test]
    fn background_latency_snapshot_ignores_stale_filter_results() {
        let mut app = test_app();
        app.benchmark_filter = "new".to_string();

        app.apply_background_latency_snapshot(Some(&BackgroundLatencySnapshot {
            selector: "select".to_string(),
            current: Some("node-a".to_string()),
            pattern: "old".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![BackgroundLatencyResult {
                name: "node-a".to_string(),
                delay: Some(88),
                completed: true,
            }],
        }));

        assert!(app.selected_benchmark().is_none());
    }

    #[test]
    fn background_latency_snapshot_does_not_overwrite_manual_latency_test() {
        let mut app = test_app();
        app.benchmark_filter = "node".to_string();
        app.prepare_node_benchmark("select", "node-a");
        let (_tx, rx) = mpsc::channel();
        app.benchmark_jobs.push(BenchmarkJob {
            group: "select".to_string(),
            nodes: vec!["node-a".to_string()],
            kind: BenchmarkJobKind::SingleNode {
                node: "node-a".to_string(),
            },
            receiver: rx,
            worker: thread::spawn(|| {}),
        });

        app.apply_background_latency_snapshot(Some(&BackgroundLatencySnapshot {
            selector: "select".to_string(),
            current: Some("node-a".to_string()),
            pattern: "node".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![BackgroundLatencyResult {
                name: "node-a".to_string(),
                delay: Some(88),
                completed: true,
            }],
        }));

        let result = app
            .selected_benchmark()
            .and_then(|summary| summary.find_result("node-a"))
            .expect("node result");
        assert_eq!(result.delay, None);
        assert!(!result.completed);
    }

    #[test]
    fn process_exists_recognizes_current_process() {
        assert!(super::process_exists(std::process::id()));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn process_alive_via_ps_recognizes_current_process() {
        assert!(process_alive_via_ps(std::process::id()));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn exited_process_is_not_alive_via_ps() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("short-lived child starts");
        let exited_pid = child.id();
        child.wait().expect("short-lived child exits");

        assert!(!process_alive_via_ps(exited_pid));
    }

    #[test]
    fn applying_filter_moves_selection_to_visible_member() {
        let mut app = test_app();
        app.groups[0].members = vec!["hk-1".to_string(), "us-1".to_string(), "hk-2".to_string()];
        app.member_index = 1;

        app.apply_benchmark_filter("hk".to_string())
            .expect("apply filter");

        assert_eq!(
            app.displayed_members(),
            vec!["hk-1".to_string(), "hk-2".to_string()]
        );
        assert_eq!(app.member_index, 0);
        assert_eq!(app.displayed_member_index(), Some(0));
    }

    #[test]
    fn displayed_members_match_any_comma_separated_filter() {
        let mut app = test_app();
        app.groups[0].members = vec![
            "美国-1".to_string(),
            "香港-1".to_string(),
            "日本-1".to_string(),
        ];

        app.apply_benchmark_filter("美国,香港".to_string())
            .expect("apply filter");

        assert_eq!(
            app.displayed_members(),
            vec!["美国-1".to_string(), "香港-1".to_string()]
        );
    }

    #[test]
    fn displayed_members_exclude_negative_filter_terms() {
        let mut app = test_app();
        app.groups[0].members = vec!["us-1".to_string(), "us-x2".to_string(), "hk-1".to_string()];

        app.apply_benchmark_filter("us,!x2".to_string())
            .expect("apply filter");

        assert_eq!(app.displayed_members(), vec!["us-1".to_string()]);
    }

    #[test]
    fn displayed_members_support_exclude_only_filter() {
        let mut app = test_app();
        app.groups[0].members = vec!["us-1".to_string(), "us-x2".to_string(), "hk-1".to_string()];

        app.apply_benchmark_filter("!x2".to_string())
            .expect("apply filter");

        assert_eq!(
            app.displayed_members(),
            vec!["us-1".to_string(), "hk-1".to_string()]
        );
    }

    #[test]
    fn auto_select_plan_selects_internet_route_even_when_node_is_kept() {
        let app = internet_routes_app();
        let group = app.group_by_name("AirTCP").expect("Internet Route").clone();
        let summary = BenchmarkSummary {
            selector: "AirTCP".to_string(),
            current: Some("air-1".to_string()),
            pattern: String::new(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![
                BenchmarkResult {
                    name: "air-1".to_string(),
                    delay: Some(80),
                    completed: true,
                },
                BenchmarkResult {
                    name: "air-2".to_string(),
                    delay: Some(90),
                    completed: true,
                },
            ],
        };

        assert_eq!(
            app.auto_select_switch_plan(&group, &summary),
            AutoPickDecision {
                target_node: None,
                parent_switch: Some(("手动选择".to_string(), "AirTCP".to_string())),
            }
        );
    }

    #[test]
    fn auto_select_uses_single_internet_route_selector_members() {
        let mut app = internet_routes_app();
        app.groups = vec![
            ProxyGroup {
                name: "手动选择".to_string(),
                kind: "Selector".to_string(),
                current: Some("airtcp".to_string()),
                members: vec!["airtcp".to_string()],
            },
            ProxyGroup {
                name: "airtcp".to_string(),
                kind: "Selector".to_string(),
                current: Some("香港-a".to_string()),
                members: vec!["香港-a".to_string(), "美国-b".to_string()],
            },
        ];
        app.internet_route_index = 0;
        app.auto_select_enabled = true;
        app.benchmark_filter = "美国".to_string();
        app.last_auto_select_benchmark = None;

        app.maybe_start_auto_select_benchmark()
            .expect("auto select starts");

        assert_eq!(app.benchmark_jobs.len(), 1);
        assert_eq!(app.benchmark_jobs[0].group, "airtcp");
        assert_eq!(app.benchmark_jobs[0].nodes, vec!["美国-b".to_string()]);
        let summary = app.benchmarks.get("airtcp").expect("airtcp summary");
        assert_eq!(summary.selector, "airtcp");
        assert_eq!(
            summary
                .results
                .iter()
                .map(|result| result.name.as_str())
                .collect::<Vec<_>>(),
            vec!["美国-b"]
        );
    }

    #[test]
    fn auto_select_toggle_allows_empty_filter() {
        let mut app = test_app();
        app.benchmark_filter.clear();

        app.handle_key(KeyCode::Char('a')).expect("toggle handled");

        assert!(app.auto_select_enabled);
        assert_eq!(
            app.status,
            "Auto-pick enabled for select (all nodes, 600ms threshold, every 30s)"
        );
    }

    #[test]
    fn benchmark_request_carries_max_concurrency() {
        let request = BenchmarkRequest {
            selector: "select".to_string(),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            request_timeout: 12.0,
            max_concurrency: 3,
            nodes: None,
        };

        assert_eq!(request.max_concurrency, 3);
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
}
