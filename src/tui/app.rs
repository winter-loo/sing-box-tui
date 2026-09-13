use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::io;
use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;

use crate::auto_pick::{
    BACKGROUND_TASK_KIND, BackgroundAutoPickManager, background_task_log_path,
    background_task_state_path, registered_status_value, stop_registered_worker,
};
use crate::automatic_selection::{
    ActiveNodeTrafficTracker, AutoSelectionExplanation, AutomaticSelectionState, NodeViewId,
    RankingPolicy,
};
use crate::benchmark_workflow::{
    BenchmarkWorkflow, QUALITY_RUNTIME_RECEIPT_ENV, QualityRuntimeReceipt,
};
use crate::config::{
    PrivateAccessRouteTableOptions, china_ip_routing_ruleset_dir, config_has_china_ip_routing,
    inspect_tailscale_config, run_private_access_route_table_config,
    run_private_access_tun_baseline_config,
};
use crate::config_mutation::paths_refer_to_same_target;
use crate::controller::{ApiClient, ConnectionsSnapshot, ProxyGroup};
use crate::defaults::{
    DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONTROLLER, DEFAULT_DELAY_TEST_URL, REFRESH_DEBOUNCE,
};
use crate::internet_tun::{InternetTunTransaction, PersistedInternetTun};
use crate::managed_sing_box::{
    AuthorizationRequirement, ControllerProbe, ManagedSingBox, sing_box_process_log_path,
    wait_for_controller_ready,
};
use crate::node_quality_path::{
    canonical_config_target, default_benchmark_db_path_for_config,
    ensure_active_config_paths_are_distinct,
};
use crate::private_access::{
    PrivateAccessSecret, PrivateAccessServiceManifest, PrivateAccessState,
    hillstone_diagnostic_log_path, sonicwall_diagnostic_log_path,
    sonicwall_gateway_profile_cache_path,
};
use crate::private_access_session::{
    PrivateAccessBridgeRouteUpdate, PrivateAccessCarrierRestart, PrivateAccessConnectOptions,
    PrivateAccessDisconnectOutcome, PrivateAccessMode, PrivateAccessNetworkIntegration,
    PrivateAccessNoticeTone, PrivateAccessProfileRuntime, PrivateAccessRuntime,
    PrivateAccessSessionNotice,
};
use crate::ruleset::china_ip_routing_ruleset_paths;
use crate::subscriptions::DEFAULT_SUBSCRIPTION_SOURCE_PATH;
use crate::sustained_quality::DEFAULT_SUSTAINED_TARGET_URL;
use crate::system_proxy::SystemProxy;
use crate::tui_state::{
    BypassRuleSetStore, OperationalWorkspace, TuiStateStore, default_tui_state_path,
    resolved_tui_bypass_rule_set_path,
};
use crate::usability_probe::{
    ManifestDiagnostic, UsabilityProbeDiscovery, UsabilityProbeManifest,
    discover_usability_probe_manifests, manifest_diagnostic, usability_probe_manifest_directory,
    with_default_usability_probe_manifests,
};

#[path = "../tui_auto_pick_worker.rs"]
mod auto_pick_worker;
#[path = "../tui_benchmark_workflow.rs"]
mod benchmark_actions;
#[path = "../tui_connections.rs"]
mod connections;
#[path = "../tui_dashboard_snapshot.rs"]
mod dashboard_snapshot;
#[path = "../tui_input_workflow.rs"]
mod input_workflow;
#[path = "../tui_managed_process.rs"]
mod managed_process;
#[path = "../tui_network_mode.rs"]
mod network_mode;
#[path = "../tui_node_quality_detail.rs"]
mod node_quality_detail;
#[path = "../tui_onboarding.rs"]
mod onboarding;
#[path = "../tui_private_access.rs"]
mod private_access_workflow;
#[path = "../tui_runtime_state.rs"]
mod runtime_state;
#[path = "../tui_selection_model.rs"]
mod selection_model;
#[path = "../tui_selection_navigation.rs"]
mod selection_navigation;
#[path = "../tui_settings.rs"]
mod settings;
#[path = "../tui_subscription_workflow.rs"]
mod subscription_workflow;
#[cfg(test)]
#[path = "../tui_test_support.rs"]
mod test_support;
#[path = "../tui_usability_probe.rs"]
mod usability_probe_workflow;
#[path = "../tui_verification.rs"]
mod verification;
#[path = "view/mod.rs"]
mod view;
use crate::process_inspection::process_is_alive as process_exists;
use settings::sonicwall_http_connect_settings;
use subscription_workflow::SubscriptionRefreshState;
use verification::{VerifyJob, default_verification_targets_setting};
#[cfg(test)]
use view::private_access_auth_display_value;
use view::{
    Focus, LeftPaneSection, NodeQualityDetailState, NodeViewPanel, OnboardingState,
    PrivateAccessAuthModal, PrivateAccessProgressEntry, PrivateAccessProgressModal,
    PrivateAccessProgressTone, SettingsEditState, help_item_count,
    private_access_auth_initial_value, private_access_progress_title, truncate_for_width,
};

const AUTO_SELECT_INTERVAL: Duration = Duration::from_secs(30);
const CONNECTION_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const NODE_QUALITY_DETAIL_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SUBSCRIPTION_REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
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
    let result = (|| {
        app.ensure_auto_pick_background_worker_if_enabled()?;
        let terminal = setup_terminal()?;
        let result = run_app(terminal, &mut app);
        let restore_result = restore_terminal();
        result.and(restore_result)
    })();
    let shutdown_result = app.shutdown_runtime_environment();
    result.and(shutdown_result)
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
    // Keep mouse reporting disabled so the host terminal retains native text
    // selection and copy behavior while the TUI is running.
    execute!(io::stdout(), EnterAlternateScreen).context("failed to enter alternate screen")?;
    Ok(ratatui::DefaultTerminal::new(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
    )?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(io::stdout(), LeaveAlternateScreen).context("failed to leave alternate screen")?;
    Ok(())
}

fn run_app(mut terminal: DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.poll_benchmark_updates()?;
        app.poll_usability_probe_updates();
        app.poll_subscription_refresh_updates()?;
        app.poll_system_proxy_updates();
        app.poll_tun_toggle_updates();
        app.poll_private_access_updates()?;
        app.poll_verify_updates();
        app.poll_background_auto_pick_status()?;
        app.maybe_start_subscription_refresh();
        app.maybe_refresh_node_quality_detail()?;
        app.maybe_refresh_connections();
        app.check_and_record_active_route();

        if !app.has_active_modal()
            && app.active_view == ActiveView::NodeList
            && app.last_user_activity.elapsed() >= Duration::from_secs(30)
        {
            app.active_view = ActiveView::IdleDashboard;
        }

        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                app.last_user_activity = Instant::now();
                if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K'))
                {
                    app.toggle_command_palette();
                    continue;
                }
                if app.command_palette.is_some() {
                    if !app.handle_command_palette_key(key.code)? {
                        return Ok(());
                    }
                    continue;
                }
                if app.active_view == ActiveView::IdleDashboard && !app.has_active_modal() {
                    match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Char('?') => {
                            app.open_help_panel();
                            continue;
                        }
                        KeyCode::Char('c') => {
                            app.show_connections = true;
                            continue;
                        }
                        KeyCode::Char('i') => {
                            let _ = app.open_node_quality_detail();
                            continue;
                        }
                        KeyCode::Enter => {
                            app.active_view = ActiveView::NodeList;
                            continue;
                        }
                        _ => {
                            // ADR 0002: unbound keys do not navigate away from dashboard
                            continue;
                        }
                    }
                }
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
    execute!(io::stdout(), EnterAlternateScreen).context("failed to re-enter alternate screen")?;
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
    if app.active_view == ActiveView::IdleDashboard {
        let snapshot = app.idle_dashboard_snapshot();
        view::render_idle_dashboard(frame, &snapshot);
    } else {
        let snapshot = app.view_snapshot();
        view::render(frame, &snapshot);

        let area = frame.area();
        if area.height > 0 && area.width > 0 {
            let header_area = ratatui::layout::Rect::new(area.x, area.y, area.width, 1);
            let theme = crate::tui::ds::Theme::default();
            let selector_name = app.selected_group().map(|g| g.name.as_str()).unwrap_or("—");
            let tun_enabled = app.internet_tun.is_enabled();
            let system_proxy_enabled = app.system_proxy.enabled();
            let clash_mode = app.clash_mode.as_deref().unwrap_or("—");

            crate::tui::ds::widgets::render_top_header(
                frame,
                header_area,
                &theme,
                app.operational_workspace,
                selector_name,
                tun_enabled,
                system_proxy_enabled,
                clash_mode,
            );
        }
    }

    if let Some(state) = &app.command_palette {
        let theme = crate::tui::ds::Theme::default();
        let filtered = view::filter_commands(&view::builtin_commands(), &state.query);
        view::render_command_palette(
            frame,
            frame.area(),
            &theme,
            &state.query,
            state.selected_index,
            &filtered,
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActiveView {
    NodeList,
    IdleDashboard,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandPaletteState {
    pub(crate) query: String,
    pub(crate) selected_index: usize,
    pub(crate) origin_view: ActiveView,
}

impl CommandPaletteState {
    pub(crate) fn new(origin_view: ActiveView) -> Self {
        Self {
            query: String::new(),
            selected_index: 0,
            origin_view,
        }
    }
}

struct App {
    client: ApiClient,
    groups: Vec<ProxyGroup>,
    group_index: usize,
    internet_route_index: usize,
    member_index: usize,
    node_view_panel: NodeViewPanel,
    focus: Focus,
    left_pane_section: LeftPaneSection,
    intranet_detail_scroll: u16,
    expanded_intranet_sections: BTreeSet<String>,
    status: String,
    flash: Option<(String, Instant)>,
    animation_started: Instant,
    benchmark_filter: String,
    benchmark_url: String,
    sustained_target_url: String,
    sustained_runtime_environment: Option<(PathBuf, PathBuf)>,
    benchmark_timeout_ms: u64,
    benchmark_request_timeout: f64,
    benchmark_max_concurrency: usize,
    verify_targets: String,
    benchmark_workflow: BenchmarkWorkflow,
    usability_probe_manifests: Vec<UsabilityProbeManifest>,
    usability_probe_diagnostics: Vec<ManifestDiagnostic>,
    usability_probe_job: Option<usability_probe_workflow::ActiveUsabilityProbe>,
    usability_probe_projection_cache:
        BTreeMap<(NodeViewId, String), crate::storage::StoredUsabilityProbeRun>,
    background_probe_enabled: BTreeSet<NodeViewId>,
    background_probe_selectors: BTreeMap<NodeViewId, String>,
    last_background_probe_started: BTreeMap<(NodeViewId, String), Instant>,
    filter_input: Option<String>,
    bypass_input: Option<String>,
    bypass_entries: Vec<String>,
    auto_select_enabled: bool,
    auto_select_selector: Option<String>,
    auto_select_node_view: NodeViewId,
    auto_select_ranking_policy: RankingPolicy,
    manual_candidate_navigation: bool,
    auto_select_interval: Duration,
    last_auto_select_benchmark: Option<Instant>,
    automatic_selection_state: AutomaticSelectionState,
    active_node_traffic: ActiveNodeTrafficTracker,
    last_auto_selection_explanation: Option<AutoSelectionExplanation>,
    background_started_at_unix: u64,
    background_auto_pick: BackgroundAutoPickManager,
    state_store: Option<TuiStateStore>,
    bypass_rule_set_store: Option<BypassRuleSetStore>,
    node_quality_detail: Option<NodeQualityDetailState>,
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
    node_quality_db_path: PathBuf,
    system_proxy: SystemProxy,
    internet_tun: InternetTunTransaction,
    china_ip_routing_enabled: bool,
    china_ip_routing_explicit: bool,
    tailscale_enabled: bool,
    tailscale_explicit: bool,
    tailscale_tailnet_domain: String,
    tailscale_hostname: String,
    verify_job: Option<VerifyJob>,
    sing_box: ManagedSingBox,
    private_access: PrivateAccessRuntime,
    private_access_progress: Option<PrivateAccessProgressModal>,
    private_access_auth: Option<PrivateAccessAuthModal>,
    metric_store: Option<crate::tui::metrics::MetricStore>,
    active_view: ActiveView,
    operational_workspace: OperationalWorkspace,
    pub(crate) command_palette: Option<CommandPaletteState>,
    last_user_activity: Instant,
    last_traffic_totals: Option<(Instant, u64, u64)>,
    last_active_traffic_rate: (String, String),
}

fn tui_persistent_path_registry(
    config_path: &std::path::Path,
    mut registry: Vec<(&'static str, PathBuf)>,
) -> Result<Vec<(&'static str, PathBuf)>> {
    let canonical_config = canonical_config_target(config_path)?;
    registry.push((
        "managed sing-box log",
        sing_box_process_log_path(&canonical_config),
    ));
    let china_ruleset_dir = china_ip_routing_ruleset_dir(config_path)?;
    let china_rulesets = china_ip_routing_ruleset_paths(&china_ruleset_dir);
    registry.push(("China routing rule-set directory", china_ruleset_dir));
    for path in china_rulesets {
        registry.push(("China routing rule-set", path));
    }
    registry.extend([
        ("SonicWall diagnostic log", sonicwall_diagnostic_log_path()),
        ("Hillstone diagnostic log", hillstone_diagnostic_log_path()),
        (
            "SonicWall gateway profile cache",
            sonicwall_gateway_profile_cache_path(),
        ),
    ]);
    Ok(registry)
}

fn validate_tui_persistent_paths(
    config_path: &std::path::Path,
    database_path: &std::path::Path,
    registry: &[(&'static str, PathBuf)],
) -> Result<()> {
    let auxiliary = registry
        .iter()
        .map(|(label, path)| (*label, path.as_path()))
        .collect::<Vec<_>>();
    ensure_active_config_paths_are_distinct(config_path, database_path, &auxiliary)
}

impl App {
    fn new(
        client: ApiClient,
        benchmark_max_concurrency: usize,
        subscription_refresh_options: TuiSubscriptionRefreshOptions,
        sing_box_executable: PathBuf,
        keep_sing_box_running: bool,
        manage_sing_box: bool,
    ) -> Result<Self> {
        let system_proxy_config_path = subscription_refresh_options.config_path.clone();
        let node_quality_db_path = default_benchmark_db_path_for_config(&system_proxy_config_path)?;
        let state_path = default_tui_state_path();
        let usability_manifest_directory =
            usability_probe_manifest_directory(&system_proxy_config_path);
        let usability_discovery = with_default_usability_probe_manifests(
            discover_usability_probe_manifests(&usability_manifest_directory).unwrap_or_else(
                |error| UsabilityProbeDiscovery {
                    manifests: Vec::new(),
                    diagnostics: vec![manifest_diagnostic(
                        &usability_manifest_directory,
                        format!("{error:#}"),
                    )],
                },
            ),
        );
        let bypass_rule_set_path = resolved_tui_bypass_rule_set_path(&system_proxy_config_path)?;
        let onboarding_subscription = PathBuf::from(DEFAULT_SUBSCRIPTION_SOURCE_PATH);
        let background_state = background_task_state_path();
        let background_log = background_task_log_path();
        // Every persistent writer reachable from App or one of its managed child processes must
        // be registered here before any state load, database open, network fetch, or file write.
        let mut persistent_paths = vec![
            (
                "subscription source",
                subscription_refresh_options.input.clone(),
            ),
            (
                "subscription cache",
                subscription_refresh_options.cache_path.clone(),
            ),
            ("TUI state", state_path.clone()),
            ("runtime bypass rule-set", bypass_rule_set_path.clone()),
            ("background task state", background_state),
            ("background task log", background_log),
        ];
        let cache_db_path = system_proxy_config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("cache.db");
        persistent_paths.push(("metric history database", cache_db_path.clone()));
        if !paths_refer_to_same_target(
            &subscription_refresh_options.input,
            &onboarding_subscription,
        )? {
            persistent_paths.push(("onboarding subscription source", onboarding_subscription));
        }
        let persistent_paths =
            tui_persistent_path_registry(&system_proxy_config_path, persistent_paths)?;
        validate_tui_persistent_paths(
            &system_proxy_config_path,
            &node_quality_db_path,
            &persistent_paths,
        )?;
        let mut metric_store = crate::tui::metrics::MetricStore::open(&cache_db_path).ok();
        if let Some(store) = &mut metric_store {
            let _ = store.load_recent_history(crate::tui::metrics::now_unix_ms());
        }
        let state_store = TuiStateStore::new(state_path);
        let existing_state_file = state_store.exists();
        let mut runtime_state = state_store.load()?;
        let onboarding_complete = runtime_state.onboarding_complete || existing_state_file;
        let sustained_runtime_environment = Some((
            system_proxy_config_path.clone(),
            sing_box_executable.clone(),
        ));
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
        let tailscale_config =
            inspect_tailscale_config(&system_proxy_config_path).unwrap_or_default();
        let subscription_refresh = SubscriptionRefreshState::from_options(
            subscription_refresh_options,
            node_quality_db_path.clone(),
        )?;
        let benchmark_workflow = BenchmarkWorkflow::open(
            client.base_url.clone(),
            client.client.clone(),
            &system_proxy_config_path,
            &node_quality_db_path,
            DEFAULT_SUSTAINED_TARGET_URL,
        )?;
        let mut app = Self {
            client,
            groups: Vec::new(),
            group_index: 0,
            internet_route_index: 0,
            member_index: 0,
            node_view_panel: NodeViewPanel::CurrentSelector,
            focus: Focus::Groups,
            left_pane_section: LeftPaneSection::Internet,
            intranet_detail_scroll: 0,
            expanded_intranet_sections: BTreeSet::new(),
            status: String::from("Loading proxy groups..."),
            flash: None,
            animation_started: Instant::now(),
            benchmark_filter: String::new(),
            benchmark_url: String::from(DEFAULT_DELAY_TEST_URL),
            sustained_target_url: String::from(DEFAULT_SUSTAINED_TARGET_URL),
            sustained_runtime_environment,
            benchmark_timeout_ms: 5000,
            benchmark_request_timeout: 12.0,
            benchmark_max_concurrency,
            verify_targets: default_verification_targets_setting(),
            benchmark_workflow,
            usability_probe_manifests: usability_discovery.manifests,
            usability_probe_diagnostics: usability_discovery.diagnostics,
            usability_probe_job: None,
            usability_probe_projection_cache: BTreeMap::new(),
            background_probe_enabled: BTreeSet::new(),
            background_probe_selectors: BTreeMap::new(),
            last_background_probe_started: BTreeMap::new(),
            filter_input: None,
            bypass_input: None,
            bypass_entries: Vec::new(),
            auto_select_enabled: false,
            auto_select_selector: None,
            auto_select_node_view: NodeViewId::current_selector(),
            auto_select_ranking_policy: RankingPolicy::Balanced,
            manual_candidate_navigation: false,
            auto_select_interval: AUTO_SELECT_INTERVAL,
            last_auto_select_benchmark: None,
            automatic_selection_state: AutomaticSelectionState::default(),
            active_node_traffic: ActiveNodeTrafficTracker::default(),
            last_auto_selection_explanation: None,
            background_started_at_unix: current_unix_timestamp(),
            background_auto_pick: Default::default(),
            state_store: Some(state_store),
            bypass_rule_set_store: Some(BypassRuleSetStore::new(bypass_rule_set_path)),
            node_quality_detail: None,
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
            node_quality_db_path,
            system_proxy,
            internet_tun,
            china_ip_routing_enabled,
            china_ip_routing_explicit: false,
            tailscale_enabled: tailscale_config.enabled,
            tailscale_explicit: tailscale_config.enabled,
            tailscale_tailnet_domain: tailscale_config.tailnet_domain.unwrap_or_default(),
            tailscale_hostname: tailscale_config.hostname.unwrap_or_default(),
            verify_job: None,
            sing_box: ManagedSingBox::new(
                sing_box_executable,
                system_proxy_config_path,
                keep_sing_box_running,
            ),
            private_access: PrivateAccessRuntime::new()?,
            private_access_progress: None,
            private_access_auth: None,
            metric_store,
            active_view: ActiveView::NodeList,
            operational_workspace: runtime_state.operational_workspace(),
            command_palette: None,
            last_user_activity: Instant::now(),
            last_traffic_totals: None,
            last_active_traffic_rate: ("0.0M/s".to_string(), "0.0M/s".to_string()),
        };
        let initialization = (|| {
            app.apply_runtime_state(runtime_state.clone())?;
            if manage_sing_box {
                app.reconcile_persisted_tun_mode(&mut runtime_state)?;
                app.reconcile_persisted_china_ip_routing()?;
                app.reconcile_persisted_tailscale()?;
                app.ensure_private_access_tun_baseline()?;
                app.authorize_tun_elevation_if_needed()?;
                app.start_managed_sing_box()?;
                app.reconcile_persisted_system_proxy()?;
            } else {
                wait_for_controller_ready(&app.client).context(
                    "headless auto-pick could not reach the existing sing-box controller",
                )?;
                let encoded_receipt = env::var(QUALITY_RUNTIME_RECEIPT_ENV)
                    .context("headless auto-pick requires a foreground runtime receipt")?;
                let runtime_receipt = QualityRuntimeReceipt::decode_from_child(&encoded_receipt)?;
                app.benchmark_workflow.adopt_runtime_receipt(
                    &app.system_proxy_config_path,
                    &app.node_quality_db_path,
                    runtime_receipt,
                )?;
            }
            app.refresh()?;
            app.restore_persisted_selections(&runtime_state)?;
            app.apply_runtime_state(runtime_state.clone())?;
            app.save_bypass_rule_set()?;
            if !app.usability_probe_diagnostics.is_empty() {
                app.set_status_only(format!(
                    "Loaded with {} invalid usability manifest(s); press ? and select the manifest diagnostics for every path and reason",
                    app.usability_probe_diagnostics.len()
                ));
            }
            Ok(())
        })();
        if let Err(error) = initialization {
            let cleanup = app.shutdown_runtime_environment();
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => {
                    error.context(format!("startup cleanup also failed: {cleanup_error:#}"))
                }
            });
        }
        Ok(app)
    }

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

    fn status_line(&self) -> String {
        self.status.clone()
    }

    pub(crate) fn has_active_modal(&self) -> bool {
        self.private_access_auth.is_some()
            || self.private_access_progress.is_some()
            || self.onboarding.is_some()
            || self.show_settings
            || self.filter_input.is_some()
            || self.bypass_input.is_some()
            || self.show_help
            || self.show_connections
            || self.node_quality_detail.is_some()
            || self.command_palette.is_some()
    }

    pub(crate) fn check_and_record_active_route(&mut self) {
        let now_ms = crate::tui::metrics::now_unix_ms();
        if let Some(group) = self.selected_group().cloned() {
            if let Some(current_node) = &group.current {
                if let Some(store) = &mut self.metric_store {
                    let route_changed = store
                        .route_intervals()
                        .last()
                        .map_or(true, |i| i.selector != group.name || i.node_name != *current_node);
                    let _ = store.record_route_switch(now_ms, &group.name, current_node);

                    let should_record_latency = route_changed
                        || store.latency_samples().last().map_or(true, |l| {
                            (now_ms - l.recorded_at_ms) >= 10_000
                        });

                    if should_record_latency {
                        let latency_ms = self
                            .benchmark_workflow
                            .reachability_assessment(&group.name, current_node)
                            .and_then(|a| {
                                a.attempts.iter().filter_map(|att| match att {
                                    crate::controller::ProbeOutcome::Reachable { delay_ms, .. } => {
                                        Some(*delay_ms)
                                    }
                                    _ => None,
                                }).last()
                            })
                            .or_else(|| {
                                self.benchmark_workflow
                                    .quick_history(&group.name, current_node)
                                    .warm_median_ms
                            });
                        if let Some(ms) = latency_ms {
                            let _ = store.record_latency(now_ms, &group.name, current_node, ms);
                        }
                    }
                }
            }
        }
    }

    fn sing_box_summary_line(&self) -> String {
        format!("sing-box: {}", self.sing_box.diagnostics())
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
        if matches!(code, KeyCode::Char('q') | KeyCode::Char('B'))
            && self.network_transition_is_running()
        {
            self.set_status_only("Wait for the network mode update before exiting");
            return Ok(true);
        }
        if self.command_palette.is_some() {
            return self.handle_command_palette_key(code);
        }
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
                KeyCode::Char('G') => {
                    self.help_index =
                        help_item_count(self.usability_probe_diagnostics.len()).saturating_sub(1)
                }
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
        if self.node_quality_detail.is_some() {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('i') => {
                    self.node_quality_detail = None;
                    self.set_status_only("Node quality detail closed");
                }
                KeyCode::Down | KeyCode::Char('j') => self.scroll_node_quality_detail_down(),
                KeyCode::Up | KeyCode::Char('k') => self.scroll_node_quality_detail_up(),
                KeyCode::Char('q') => return Ok(false),
                _ => {}
            }
            return Ok(true);
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc if self.network_transition_is_running() => {
                self.set_status_only("Wait for the network mode update before exiting");
            }
            KeyCode::Esc if self.is_any_probe_running() => {
                self.pause_active_probes();
            }
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Tab => {
                self.last_user_activity = Instant::now();
                self.cycle_operational_workspace()?;
            }
            KeyCode::Right if self.focus == Focus::Members => self.move_node_view_next(),
            KeyCode::Left if self.focus == Focus::Members => self.move_node_view_previous(),
            KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Members,
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Groups,
            KeyCode::Down | KeyCode::Char('j') => self.move_next(),
            KeyCode::Up | KeyCode::Char('k') => self.move_previous(),
            KeyCode::Char('g') => self.move_first(),
            KeyCode::Char('G') => self.move_last(),
            KeyCode::Char('r') => self.refresh()?,
            KeyCode::Char('u') => self.start_manual_subscription_refresh(),
            KeyCode::Char('U') => self.start_manual_usability_probe(),
            KeyCode::Char('P') => self.toggle_background_usability_probe()?,
            KeyCode::Char('T') => self.start_group_benchmark()?,
            KeyCode::Char('t') => self.start_member_benchmark()?,
            KeyCode::Char('a') => self.toggle_auto_select()?,
            KeyCode::Char('m') => self.cycle_clash_mode()?,
            KeyCode::Char('b') => self.open_bypass_modal(),
            KeyCode::Char('B') => return self.keep_sing_box_running_in_background(),
            KeyCode::Char('p') => self.set_system_proxy(),
            KeyCode::Char('\\') => self.toggle_tun_mode(),
            KeyCode::Char('i') => self.open_node_quality_detail()?,
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

    fn selected_member_name(&self) -> Option<String> {
        let displayed = self.displayed_members();
        let index = self.displayed_member_index()?;
        displayed.get(index).cloned()
    }

    pub(crate) fn toggle_command_palette(&mut self) {
        if let Some(state) = self.command_palette.take() {
            self.active_view = state.origin_view;
        } else {
            self.command_palette = Some(CommandPaletteState::new(self.active_view));
        }
    }

    pub(crate) fn close_command_palette(&mut self) {
        if let Some(state) = self.command_palette.take() {
            self.active_view = state.origin_view;
        }
    }

    pub(crate) fn handle_command_palette_key(&mut self, code: KeyCode) -> Result<bool> {
        if matches!(code, KeyCode::Esc) {
            self.close_command_palette();
            return Ok(true);
        }

        let Some(mut state) = self.command_palette.take() else {
            return Ok(true);
        };

        match code {
            KeyCode::Esc => unreachable!(),
            KeyCode::Up => {
                state.selected_index = state.selected_index.saturating_sub(1);
                self.command_palette = Some(state);
            }
            KeyCode::Down => {
                let filtered = view::filter_commands(&view::builtin_commands(), &state.query);
                if !filtered.is_empty() && state.selected_index + 1 < filtered.len() {
                    state.selected_index += 1;
                }
                self.command_palette = Some(state);
            }
            KeyCode::Backspace => {
                state.query.pop();
                let filtered = view::filter_commands(&view::builtin_commands(), &state.query);
                if state.selected_index >= filtered.len() {
                    state.selected_index = filtered.len().saturating_sub(1);
                }
                self.command_palette = Some(state);
            }
            KeyCode::Char(ch) => {
                state.query.push(ch);
                state.selected_index = 0;
                self.command_palette = Some(state);
            }
            KeyCode::Enter => {
                let filtered = view::filter_commands(&view::builtin_commands(), &state.query);
                if let Some(item) = filtered.get(state.selected_index) {
                    let action_id = item.id;
                    self.command_palette = None;
                    return self.execute_command(action_id);
                }
                self.command_palette = Some(state);
            }
            _ => {
                self.command_palette = Some(state);
            }
        }
        Ok(true)
    }

    pub(crate) fn execute_command(&mut self, action_id: &str) -> Result<bool> {
        match action_id {
            view::CMD_SWITCH_INTERNET => {
                self.set_operational_workspace(OperationalWorkspace::Internet)?;
                self.active_view = ActiveView::NodeList;
            }
            view::CMD_SWITCH_PRIVATE_ACCESS => {
                self.set_operational_workspace(OperationalWorkspace::PrivateAccess)?;
                self.active_view = ActiveView::NodeList;
            }
            view::CMD_TOGGLE_TUN => {
                self.toggle_tun_mode();
            }
            view::CMD_TOGGLE_SYSTEM_PROXY => {
                self.set_system_proxy();
            }
            view::CMD_TRIGGER_USABILITY_PROBES => {
                self.start_manual_usability_probe();
            }
            view::CMD_REFRESH_SUBSCRIPTIONS => {
                self.start_manual_subscription_refresh();
            }
            view::CMD_REFRESH_CONNECTIONS => {
                self.last_connection_refresh = Instant::now() - CONNECTION_REFRESH_INTERVAL;
                self.maybe_refresh_connections();
                self.set_status_only("Connection details refreshed");
            }
            view::CMD_VIEW_CONNECTIONS => {
                self.open_connections_panel();
            }
            view::CMD_VIEW_NODE_QUALITY => {
                let _ = self.open_node_quality_detail();
            }
            view::CMD_OPEN_SETTINGS => {
                self.open_settings_panel();
            }
            view::CMD_OPEN_HELP => {
                self.open_help_panel();
            }
            view::CMD_ENTER_IDLE_DASHBOARD => {
                self.active_view = ActiveView::IdleDashboard;
            }
            view::CMD_QUIT => {
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    }

    pub(crate) fn set_operational_workspace(
        &mut self,
        workspace: OperationalWorkspace,
    ) -> Result<()> {
        self.operational_workspace = workspace;
        match self.operational_workspace {
            OperationalWorkspace::Internet => {
                self.left_pane_section = LeftPaneSection::Internet;
                self.set_status_only("Switched to Internet workspace");
            }
            OperationalWorkspace::PrivateAccess => {
                if self.private_access.is_configured() {
                    self.left_pane_section = LeftPaneSection::Intranet;
                }
                self.set_status_only("Switched to Private Access workspace");
            }
        }
        self.save_runtime_state()?;
        Ok(())
    }

    pub(crate) fn cycle_operational_workspace(&mut self) -> Result<()> {
        let next = self.operational_workspace.cycle();
        self.set_operational_workspace(next)
    }

        pub(super) fn is_any_probe_running(&self) -> bool {
        self.usability_probe_job.is_some() || self.benchmark_workflow.is_running()
    }

    pub(super) fn pause_active_probes(&mut self) {
        let mut stopped = false;
        if self.usability_probe_job.is_some() {
            if let Err(error) = self.cancel_active_usability_probe_with_reason("User paused probe") {
                self.set_status_only(format!("Cannot pause usability probe: {error:#}"));
            } else {
                stopped = true;
            }
        }
        if self.benchmark_workflow.is_running() {
            self.benchmark_workflow.cancel_running_probes();
            stopped = true;
        }
        if stopped {
            self.manual_candidate_navigation = true;
            self.set_status_only("Probe paused");
        }
    }

fn open_help_panel(&mut self) {
        self.show_help = true;
        self.flash = None;
        self.set_status_only("Showing help");
    }

    fn move_help_next(&mut self) {
        self.help_index = (self.help_index + 1)
            .min(help_item_count(self.usability_probe_diagnostics.len()).saturating_sub(1));
    }

    fn move_help_previous(&mut self) {
        self.help_index = self.help_index.saturating_sub(1);
    }
}

#[cfg(test)]
#[path = "../tui_interaction_tests.rs"]
mod interaction_tests;
#[cfg(test)]
#[path = "../tui_runtime_integration_tests.rs"]
mod runtime_integration_tests;

#[cfg(test)]
mod persistent_path_tests {
    use super::{tui_persistent_path_registry, validate_tui_persistent_paths};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-app-paths-{nonce}"))
    }

    #[test]
    fn tui_writers_are_rejected_before_they_can_alias_quality_storage() {
        for role in [
            "input",
            "onboarding",
            "state",
            "bypass",
            "background",
            "background-self",
            "managed-log",
            "ruleset",
            "private-access",
        ] {
            let dir = temp_dir().join(role);
            fs::create_dir_all(&dir).expect("create temp dir");
            let config = dir.join("config.json");
            let database = match role {
                "managed-log" => dir.join("sing-box.log"),
                "ruleset" => dir.join("sing-box-tui-rulesets/geoip-cn.srs"),
                _ => dir.join("quality.sqlite3"),
            };
            let alias = match role {
                "input" | "onboarding" => dir.join("quality.sqlite3-shm"),
                "state" => database.clone(),
                "bypass" => dir.join("quality.sqlite3.node-quality-writes-blocked"),
                "background" => dir.join("quality.sqlite3-journal"),
                "background-self" => dir.join("background.log"),
                "managed-log" | "ruleset" => database.clone(),
                "private-access" => dir.join("quality.sqlite3-wal"),
                _ => unreachable!(),
            };
            fs::write(&config, b"{}\n").expect("write active config");
            if let Some(parent) = alias.parent() {
                fs::create_dir_all(parent).expect("create alias parent");
            }
            fs::write(&alias, b"canary\n").expect("write protected canary");
            let input = if role == "input" {
                alias.clone()
            } else {
                dir.join("subscriptions.txt")
            };
            let state = if role == "state" {
                alias.clone()
            } else {
                dir.join("state.json")
            };
            let bypass = if role == "bypass" {
                alias.clone()
            } else {
                dir.join("bypass.json")
            };
            let background_state = if matches!(role, "background" | "background-self") {
                alias.clone()
            } else {
                dir.join("background.json")
            };
            let background_log = if role == "background-self" {
                alias.clone()
            } else {
                dir.join("background.log")
            };
            let onboarding = if role == "onboarding" {
                alias.clone()
            } else {
                dir.join("onboarding.suburl")
            };

            let mut registry = vec![
                ("subscription source", input),
                ("subscription cache", dir.join("cache.json")),
                ("TUI state", state),
                ("runtime bypass rule-set", bypass),
                ("background task state", background_state),
                ("background task log", background_log),
            ];
            if role == "private-access" {
                registry.push(("SonicWall diagnostic log", alias.clone()));
            }
            if registry[0].1 != onboarding {
                registry.push(("onboarding subscription source", onboarding));
            }
            let registry =
                tui_persistent_path_registry(&config, registry).expect("derive TUI writer paths");
            let error = validate_tui_persistent_paths(&config, &database, &registry)
                .expect_err("TUI writer alias must fail before startup I/O");
            assert!(format!("{error:#}").contains("must not alias"));
            assert_eq!(fs::read(&alias).expect("read canary"), b"canary\n");
            assert!(!database.exists() || database == alias);
            let _ = fs::remove_dir_all(dir.parent().expect("role directory parent"));
        }
    }

    #[test]
    fn fresh_tui_writer_directories_are_validated_without_being_created() {
        let dir = temp_dir().join("fresh-startup");
        fs::create_dir_all(&dir).expect("create config directory");
        let config = dir.join("config.json");
        let database = dir.join("quality.sqlite3");
        let future_cache = dir.join("future/cache/subscriptions.json");
        let future_background = dir.join("future/background/state.json");
        let registry = tui_persistent_path_registry(
            &config,
            vec![
                ("subscription source", dir.join("future/source/.suburl")),
                ("subscription cache", future_cache.clone()),
                ("TUI state", dir.join("future/state/tui.json")),
                (
                    "runtime bypass rule-set",
                    dir.join("future/bypass/rules.json"),
                ),
                ("background task state", future_background.clone()),
                (
                    "background task log",
                    dir.join("future/background/state.log"),
                ),
                (
                    "onboarding subscription source",
                    dir.join("future/onboarding/.suburl"),
                ),
                (
                    "SonicWall gateway profile cache",
                    dir.join("future/private-access/profiles.json"),
                ),
            ],
        )
        .expect("derive fresh writer registry");

        validate_tui_persistent_paths(&config, &database, &registry)
            .expect("missing writer parents are valid future targets");
        assert!(!dir.join("future").exists());
        assert!(!dir.join("sing-box-tui-rulesets").exists());
        assert!(!database.exists());
        let _ = fs::remove_dir_all(dir.parent().expect("temporary root"));
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::*;
    use std::fs;
    use std::thread;
    use crossterm::event::KeyCode;
    use crate::tui_state::{OperationalWorkspace, TuiRuntimeState, TuiStateStore};

    #[test]
    fn tab_cycles_operational_workspace_and_persists() {
        let mut app = test_support::test_app();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sing-box-tui-tab-test-{nonce}.json"));
        let store = TuiStateStore::new(&path);
        app.state_store = Some(store.clone());

        assert_eq!(app.operational_workspace, OperationalWorkspace::Internet);
        assert_eq!(app.left_pane_section, LeftPaneSection::Internet);

        let initial_activity = app.last_user_activity;
        thread::sleep(Duration::from_millis(5));

        // Tab -> PrivateAccess
        app.handle_key(KeyCode::Tab).expect("tab handled");
        assert_eq!(app.operational_workspace, OperationalWorkspace::PrivateAccess);
        assert_eq!(app.left_pane_section, LeftPaneSection::Intranet);
        assert!(app.last_user_activity > initial_activity);
        let persisted = store.load().expect("load persisted state");
        assert_eq!(persisted.operational_workspace.as_deref(), Some("private_access"));

        let next_activity = app.last_user_activity;
        thread::sleep(Duration::from_millis(5));

        // Tab -> Internet
        app.handle_key(KeyCode::Tab).expect("tab handled");
        assert_eq!(app.operational_workspace, OperationalWorkspace::Internet);
        assert_eq!(app.left_pane_section, LeftPaneSection::Internet);
        assert!(app.last_user_activity > next_activity);
        let persisted = store.load().expect("load persisted state");
        assert_eq!(persisted.operational_workspace.as_deref(), Some("internet"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn restoring_workspace_does_not_connect_vpn_or_switch_selector() {
        let mut app = test_support::test_app();
        // Initial selector state
        assert_eq!(app.groups[0].name, "select");
        assert_eq!(app.groups[0].current.as_deref(), Some("node-a"));

        // Simulate state with PrivateAccess restored
        let mut state = app.runtime_state();
        state.operational_workspace = Some("private_access".to_string());
        state.current_selected_nodes.insert("select".to_string(), "node-a".to_string());

        app.apply_runtime_state(state).expect("apply runtime state");

        // Workspace restored
        assert_eq!(app.operational_workspace, OperationalWorkspace::PrivateAccess);
        // Selector must NOT have changed
        assert_eq!(app.groups[0].current.as_deref(), Some("node-a"));
        // VPN must NOT be connected or connecting
        for profile in &app.private_access.profiles {
            assert_eq!(profile.state, crate::private_access::PrivateAccessState::Disconnected);
            assert_ne!(profile.state, crate::private_access::PrivateAccessState::Connected);
            assert_ne!(profile.state, crate::private_access::PrivateAccessState::Connecting);
        }
    }

    #[test]
    fn unconfigured_private_access_defaults_restored_workspace_to_internet() {
        let mut app = test_support::test_app_without_private_access();
        let mut state = TuiRuntimeState::default();
        state.operational_workspace = Some("private_access".to_string());

        app.apply_runtime_state(state).expect("apply runtime state");

        // Since private_access is not configured, fall back to Internet workspace
        assert_eq!(app.operational_workspace, OperationalWorkspace::Internet);
        assert_eq!(app.left_pane_section, LeftPaneSection::Internet);
    }

    #[test]
    fn operational_view_draw_renders_top_header() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = test_support::test_app();
        app.active_view = ActiveView::NodeList;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let mut text = String::new();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }

        assert!(text.contains("SING-BOX TUI · INTERNET · select"));
        assert!(text.contains("[Tab] Switch Workspace"));
        assert!(text.contains("[Ctrl+K] Actions"));
        assert!(text.contains("TUN: [OFF]"));
        assert!(text.contains("SYS PROXY: [OFF]"));
        assert!(text.contains("CLASH: [RULE]"));
    }

    #[test]
    fn command_palette_toggle_and_esc_lifecycle() {
        let mut app = test_support::test_app();
        app.active_view = ActiveView::NodeList;
        assert!(app.command_palette.is_none());
        assert!(!app.has_active_modal());

        // Open palette
        app.toggle_command_palette();
        assert!(app.command_palette.is_some());
        assert_eq!(
            app.command_palette.as_ref().unwrap().origin_view,
            ActiveView::NodeList
        );
        assert_eq!(app.command_palette.as_ref().unwrap().query, "");
        assert_eq!(app.command_palette.as_ref().unwrap().selected_index, 0);
        assert!(app.has_active_modal());

        // Toggle closes palette
        app.toggle_command_palette();
        assert!(app.command_palette.is_none());
        assert_eq!(app.active_view, ActiveView::NodeList);
        assert!(!app.has_active_modal());

        // Open from IdleDashboard and close with Esc
        app.active_view = ActiveView::IdleDashboard;
        app.toggle_command_palette();
        assert!(app.command_palette.is_some());
        assert_eq!(
            app.command_palette.as_ref().unwrap().origin_view,
            ActiveView::IdleDashboard
        );

        let res = app.handle_command_palette_key(KeyCode::Esc).unwrap();
        assert!(res);
        assert!(app.command_palette.is_none());
        assert_eq!(app.active_view, ActiveView::IdleDashboard);
    }

    #[test]
    fn command_palette_query_typing_and_navigation() {
        let mut app = test_support::test_app();
        app.toggle_command_palette();

        // Navigate Down
        app.handle_command_palette_key(KeyCode::Down).unwrap();
        assert_eq!(app.command_palette.as_ref().unwrap().selected_index, 1);

        // Navigate Up
        app.handle_command_palette_key(KeyCode::Up).unwrap();
        assert_eq!(app.command_palette.as_ref().unwrap().selected_index, 0);

        // Typing character appends and resets selected_index
        app.handle_command_palette_key(KeyCode::Down).unwrap();
        assert_eq!(app.command_palette.as_ref().unwrap().selected_index, 1);

        app.handle_command_palette_key(KeyCode::Char('t')).unwrap();
        app.handle_command_palette_key(KeyCode::Char('u')).unwrap();
        app.handle_command_palette_key(KeyCode::Char('n')).unwrap();
        assert_eq!(app.command_palette.as_ref().unwrap().query, "tun");
        assert_eq!(app.command_palette.as_ref().unwrap().selected_index, 0);

        // Backspace pops character
        app.handle_command_palette_key(KeyCode::Backspace).unwrap();
        assert_eq!(app.command_palette.as_ref().unwrap().query, "tu");
    }

    #[test]
    fn command_palette_execution_workspace_switch() {
        let mut app = test_support::test_app();
        app.operational_workspace = OperationalWorkspace::Internet;
        app.toggle_command_palette();

        // Type query for private access
        for c in "private".chars() {
            app.handle_command_palette_key(KeyCode::Char(c)).unwrap();
        }
        let filtered = view::filter_commands(
            &view::builtin_commands(),
            &app.command_palette.as_ref().unwrap().query,
        );
        assert!(!filtered.is_empty());
        assert_eq!(filtered[0].id, view::CMD_SWITCH_PRIVATE_ACCESS);

        // Press Enter
        app.handle_command_palette_key(KeyCode::Enter).unwrap();
        assert!(app.command_palette.is_none());
        assert_eq!(
            app.operational_workspace,
            OperationalWorkspace::PrivateAccess
        );
    }

    #[test]
    fn command_palette_execution_overlay_and_views() {
        let mut app = test_support::test_app();

        // Open settings via command palette
        app.toggle_command_palette();
        for c in "settings".chars() {
            app.handle_command_palette_key(KeyCode::Char(c)).unwrap();
        }
        app.handle_command_palette_key(KeyCode::Enter).unwrap();
        assert!(app.command_palette.is_none());
        assert!(app.show_settings);

        // Open connections via command palette
        app.toggle_command_palette();
        for c in "view conn".chars() {
            app.handle_command_palette_key(KeyCode::Char(c)).unwrap();
        }
        app.handle_command_palette_key(KeyCode::Enter).unwrap();
        assert!(app.command_palette.is_none());
        assert!(app.show_connections);

        // Enter idle dashboard
        app.toggle_command_palette();
        for c in "idle".chars() {
            app.handle_command_palette_key(KeyCode::Char(c)).unwrap();
        }
        app.handle_command_palette_key(KeyCode::Enter).unwrap();
        assert!(app.command_palette.is_none());
        assert_eq!(app.active_view, ActiveView::IdleDashboard);

        // Quit command returns Ok(false)
        app.toggle_command_palette();
        for c in "quit".chars() {
            app.handle_command_palette_key(KeyCode::Char(c)).unwrap();
        }
        let quit_result = app.handle_command_palette_key(KeyCode::Enter).unwrap();
        assert!(!quit_result);
        assert!(app.command_palette.is_none());
    }

    #[test]
    fn command_palette_draw_over_view() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = test_support::test_app();
        app.toggle_command_palette();
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let mut text = String::new();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }

        assert!(text.contains("COMMAND PALETTE (Ctrl+K)"));
        assert!(text.contains("> █"));
        assert!(text.contains("[Enter] Execute  [Esc] Dismiss"));
    }
}

