use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read as IoRead, Seek, SeekFrom, Write};
use std::net::{SocketAddr, SocketAddrV4, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use std::process::{Child, Command, Stdio};
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
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Clear, Dataset, GraphType, List, ListItem, ListState, Paragraph,
    Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroize;

use crate::config::{
    PrivateAccessRouteTableOptions, run_private_access_carrier_route_config,
    run_private_access_route_table_config,
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
use crate::private_access::{
    ExternalPrivateAccessService, PrivateAccessAuthField, PrivateAccessBridge,
    PrivateAccessCommand, PrivateAccessEvent, PrivateAccessRoute, PrivateAccessSecret,
    PrivateAccessServiceManifest, PrivateAccessState, default_hillstone_manifest,
    default_sonicwall_manifest, load_private_access_manifest,
};
use crate::storage::{
    BenchmarkRecord, BenchmarkStore, NodeLatencySample, default_benchmark_db_path,
};
use crate::subscriptions::{
    DEFAULT_SUBSCRIPTION_SOURCE_PATH, SubscriptionRefreshOutput, SubscriptionRefreshRequest,
    refresh_subscriptions,
};
use crate::tui_state::{
    BypassRuleSetStore, PrivateAccessProfileState, TuiRuntimeState, TuiStateStore,
    default_bypass_rule_set_path, default_tui_state_path, parse_bypass_entries,
};

const AUTO_SELECT_INTERVAL: Duration = Duration::from_secs(30);
const AUTO_SELECT_THRESHOLD_MS: u64 = 600;
const CONNECTION_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LATENCY_CHART_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SYSTEM_PROXY_STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const SUBSCRIPTION_REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const LATENCY_CHART_DEFAULT_WINDOW: Duration = Duration::from_secs(60 * 60);
const LATENCY_CHART_MIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const LATENCY_CHART_MAX_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const BACKGROUND_TASK_KIND_AUTO_PICK: &str = "headless-auto-pick";
const BACKGROUND_TASK_PATH: &str = "sing-box-tui-background.json";
const BACKGROUND_REGISTRY_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKGROUND_STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const PRIVATE_ACCESS_EVENTS_PER_POLL: usize = 64;
// Keep RFC1918 ranges out of the OS-level bypass list. The Hillstone bridge
// problem showed why this matters: if macOS bypasses 10.* before traffic reaches
// sing-box, sing-box cannot apply its route override to the local ESP bridge.
// Private/LAN destinations should enter sing-box first and then use direct rules.
const DEFAULT_SYSTEM_PROXY_BYPASS: &[&str] = &["localhost", "127.*"];
const DIRECT_CLASH_MODE: &str = "直连";
const RULE_CLASH_MODE: &str = "规则";
const GLOBAL_CLASH_MODE: &str = "全局";
const SETTINGS_FIELDS: &[SettingsField] = &[
    SettingsField::BenchmarkUrl,
    SettingsField::BenchmarkTimeoutMs,
    SettingsField::RequestTimeoutSec,
    SettingsField::MaxConcurrency,
    SettingsField::VerifyTargets,
    SettingsField::AutoPickThresholdMs,
    SettingsField::AutoPickIntervalSec,
    SettingsField::SystemProxyServer,
    SettingsField::PrivateAccessProfile,
    SettingsField::PrivateAccessManifestPath,
    SettingsField::PrivateAccessMode,
    SettingsField::PrivateAccessServer,
    SettingsField::PrivateAccessPort,
    SettingsField::PrivateAccessUsername,
    SettingsField::PrivateAccessPassword,
    SettingsField::PrivateAccessPasswordEnv,
    SettingsField::PrivateAccessBridgeListen,
    SettingsField::PrivateAccessTlsVerify,
];

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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BackgroundAutoPickConfig {
    enabled: bool,
    selector: Option<String>,
    filter: String,
    benchmark_url: String,
    timeout_ms: u64,
    request_timeout: f64,
    max_concurrency: usize,
    threshold_ms: u64,
    interval_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BackgroundWorkerCommand {
    Status,
    ApplyConfig { config: BackgroundAutoPickConfig },
    Stop,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BackgroundControlRequest {
    token: String,
    #[serde(flatten)]
    command: BackgroundWorkerCommand,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BackgroundControlResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<BackgroundStatusSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BackgroundStatusSnapshot {
    kind: String,
    pid: u32,
    controller: String,
    config_path: PathBuf,
    max_concurrency: usize,
    started_at_unix: u64,
    status_generation: u64,
    worker_status: String,
    updated_at_unix: u64,
    auto_pick_enabled: bool,
    auto_pick_selector: Option<String>,
    filter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latency: Option<BackgroundLatencySnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BackgroundLatencySnapshot {
    selector: String,
    current: Option<String>,
    pattern: String,
    url: String,
    timeout_ms: u64,
    max_concurrency: usize,
    results: Vec<BackgroundLatencyResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BackgroundLatencyResult {
    name: String,
    delay: Option<u64>,
    completed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BackgroundTaskState {
    #[allow(dead_code)]
    version: u8,
    kind: String,
    pid: u32,
    #[allow(dead_code)]
    controller: String,
    #[allow(dead_code)]
    config_path: PathBuf,
    #[allow(dead_code)]
    max_concurrency: usize,
    #[allow(dead_code)]
    started_at_unix: u64,
    #[serde(default)]
    #[allow(dead_code)]
    status_generation: u64,
    #[serde(default)]
    #[allow(dead_code)]
    status: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    updated_at_unix: Option<u64>,
    bind_addr: String,
    token: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BackgroundTaskEnsureResult {
    AlreadyRunning(u32),
    Started(u32),
}

impl BackgroundTaskEnsureResult {
    fn pid(&self) -> u32 {
        match self {
            Self::AlreadyRunning(pid) | Self::Started(pid) => *pid,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::AlreadyRunning(_) => "running",
            Self::Started(_) => "started",
        }
    }
}

struct BackgroundWorkerRuntime {
    pid: u32,
    bind_addr: String,
    token: String,
    child: Option<Child>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BackgroundStatusTarget {
    pid: u32,
    bind_addr: String,
    token: String,
}

struct BackgroundStatusPollOutcome {
    result: Result<BackgroundStatusSnapshot, String>,
    process_alive: bool,
}

enum BackgroundStatusPollResolution {
    Snapshot(Box<BackgroundStatusSnapshot>),
    Retry(String),
    Reconnect(String),
}

struct BackgroundStatusPollJob {
    target: BackgroundStatusTarget,
    receiver: mpsc::Receiver<BackgroundStatusPollOutcome>,
    worker: JoinHandle<()>,
}

fn resolve_background_status_poll(
    outcome: BackgroundStatusPollOutcome,
) -> BackgroundStatusPollResolution {
    match outcome.result {
        Ok(snapshot) => BackgroundStatusPollResolution::Snapshot(Box::new(snapshot)),
        Err(error) if outcome.process_alive => BackgroundStatusPollResolution::Retry(error),
        Err(error) => BackgroundStatusPollResolution::Reconnect(error),
    }
}

struct BackgroundWorkerRequest {
    command: BackgroundWorkerCommand,
    response: mpsc::Sender<BackgroundControlResponse>,
}

pub(crate) fn run_tui(
    controller: Option<String>,
    max_concurrency: Option<usize>,
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
        true,
        false,
    )?;
    app.run_headless_auto_pick_loop()
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) fn run_background_status() -> Result<()> {
    let Some(state) = read_background_task_state()? else {
        print_json(serde_json::json!({
            "status": "none",
        }))?;
        return Ok(());
    };
    let snapshot =
        match send_background_control_request_to_state(&state, BackgroundWorkerCommand::Status) {
            Ok(snapshot) => snapshot,
            Err(_) if !process_exists(state.pid) => {
                remove_background_task_state_file();
                print_json(serde_json::json!({
                    "status": "stale",
                    "kind": state.kind,
                    "pid": state.pid,
                }))?;
                return Ok(());
            }
            Err(error) => {
                return Err(error).context("failed to query live background worker over TCP");
            }
        };
    let mut value = serde_json::to_value(snapshot).context("failed to encode background status")?;
    if let Some(object) = value.as_object_mut() {
        object.insert("status".to_string(), Value::String("running".to_string()));
        object.insert("bind_addr".to_string(), Value::String(state.bind_addr));
    }
    print_json(value)?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub(crate) fn run_background_status() -> Result<()> {
    bail!("background process status is only available on Windows, macOS, and Linux")
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) fn run_background_stop() -> Result<()> {
    let Some(pid) = stop_registered_background_auto_pick_task()? else {
        disable_persisted_auto_pick()?;
        print_json(serde_json::json!({ "status": "none" }))?;
        return Ok(());
    };
    disable_persisted_auto_pick()?;
    print_json(serde_json::json!({
        "status": "stopped",
        "kind": BACKGROUND_TASK_KIND_AUTO_PICK,
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

fn background_task_state_path() -> PathBuf {
    env::var("SING_BOX_TUI_BACKGROUND")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(BACKGROUND_TASK_PATH))
}

fn background_task_log_path() -> PathBuf {
    background_task_state_path().with_extension("log")
}

fn read_text_tail(path: &Path, max_bytes: usize) -> Option<String> {
    if max_bytes == 0 {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length == 0 {
        return None;
    }
    let read_len = length.min(max_bytes as u64) as usize;
    file.seek(SeekFrom::Start(length.saturating_sub(read_len as u64)))
        .ok()?;
    let mut buffer = vec![0; read_len];
    file.read_exact(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn background_log_tail_context(log_path: &Path) -> String {
    read_text_tail(log_path, 16 * 1024)
        .map(|tail| format!("; background worker stderr tail: {tail}"))
        .unwrap_or_default()
}

fn read_background_task_state() -> Result<Option<BackgroundTaskState>> {
    let path = background_task_state_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read background state {}", path.display()))?;
    let state = serde_json::from_str::<BackgroundTaskState>(&text)
        .with_context(|| format!("failed to parse background state {}", path.display()))?;
    if state.kind != BACKGROUND_TASK_KIND_AUTO_PICK {
        bail!(
            "unsupported background task kind '{}' in {}",
            state.kind,
            path.display()
        );
    }
    Ok(Some(state))
}

fn remove_background_task_state_file() {
    let path = background_task_state_path();
    let _ = fs::remove_file(path);
}

fn write_background_task_state(state: &BackgroundTaskState) -> Result<()> {
    let path = background_task_state_path();
    write_background_task_state_to_path(&path, state)
}

fn write_background_task_state_to_path(path: &Path, state: &BackgroundTaskState) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create background task state directory {}",
                parent.display()
            )
        })?;
    }
    let text =
        serde_json::to_string_pretty(state).context("failed to encode background task state")?;
    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        bail!(
            "refusing to write background task state through symlink {}",
            path.display()
        );
    }
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .with_context(|| format!("failed to open background task state {}", path.display()))?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| {
            format!(
                "failed to restrict background task state permissions {}",
                path.display()
            )
        })?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to write background task state {}", path.display()))
}

fn stop_background_auto_pick_task() -> Result<()> {
    stop_registered_background_auto_pick_task().map(|_| ())
}

fn stop_registered_background_auto_pick_task() -> Result<Option<u32>> {
    let Some(state) = read_background_task_state()? else {
        return Ok(None);
    };
    let pid = state.pid;
    if process_exists(state.pid) {
        let stopped =
            send_background_control_request_to_state(&state, BackgroundWorkerCommand::Stop)
                .and_then(|_| {
                    wait_for_background_process_to_exit(state.pid, Duration::from_secs(3))
                })
                .is_ok();
        if !stopped {
            stop_background_pid(state.pid).with_context(|| {
                format!("failed to stop background auto-pick pid {}", state.pid)
            })?;
        }
    }
    remove_background_task_state_file();
    Ok(Some(pid))
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

fn background_bind_addr() -> String {
    env::var("SING_BOX_TUI_BACKGROUND_BIND")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1:0".to_string())
}

fn background_remote_bind_allowed() -> bool {
    env::var("SING_BOX_TUI_BACKGROUND_ALLOW_REMOTE")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

fn background_token_from_env() -> String {
    env::var("SING_BOX_TUI_BACKGROUND_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(random_background_token)
}

fn random_background_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn spawn_background_tcp_server(
    bind_addr: &str,
    token: String,
) -> Result<(SocketAddr, mpsc::Receiver<BackgroundWorkerRequest>)> {
    validate_background_bind_addr(bind_addr)?;
    let listener = TcpListener::bind(bind_addr)
        .with_context(|| format!("failed to bind background TCP control listener {bind_addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read background TCP listener address")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            let tx = tx.clone();
            let token = token.clone();
            thread::spawn(move || {
                let _ = handle_background_tcp_connection(stream, &token, tx);
            });
        }
    });
    Ok((local_addr, rx))
}

fn validate_background_bind_addr(bind_addr: &str) -> Result<()> {
    validate_background_bind_addr_with_remote(bind_addr, background_remote_bind_allowed())
}

fn validate_background_bind_addr_with_remote(bind_addr: &str, allow_remote: bool) -> Result<()> {
    let addr = bind_addr
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid background TCP bind address: {bind_addr}"))?;
    if addr.ip().is_loopback() || allow_remote {
        return Ok(());
    }
    bail!(
        "refusing non-loopback background TCP bind address {bind_addr}; set SING_BOX_TUI_BACKGROUND_ALLOW_REMOTE=1 to allow remote management"
    )
}

fn handle_background_tcp_connection(
    mut stream: TcpStream,
    token: &str,
    tx: mpsc::Sender<BackgroundWorkerRequest>,
) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("failed to set background TCP read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("failed to set background TCP write timeout")?;
    let mut line = String::new();
    BufReader::new(stream.try_clone().context("failed to clone TCP stream")?)
        .read_line(&mut line)
        .context("failed to read background TCP request")?;
    let request = serde_json::from_str::<BackgroundControlRequest>(&line)
        .context("failed to parse background TCP request")?;
    if request.token != token {
        let response = BackgroundControlResponse {
            ok: false,
            error: Some("unauthorized".to_string()),
            status: None,
        };
        write_background_control_response(&mut stream, &response)?;
        return Ok(());
    }
    let (response_tx, response_rx) = mpsc::channel();
    tx.send(BackgroundWorkerRequest {
        command: request.command,
        response: response_tx,
    })
    .context("background worker control loop is not available")?;
    let response = response_rx
        .recv_timeout(Duration::from_secs(5))
        .context("timed out waiting for background control response")?;
    write_background_control_response(&mut stream, &response)
}

fn write_background_control_response(
    stream: &mut TcpStream,
    response: &BackgroundControlResponse,
) -> Result<()> {
    let text = serde_json::to_string(response).context("failed to encode background response")?;
    writeln!(stream, "{text}").context("failed to write background response")?;
    stream
        .flush()
        .context("failed to flush background response")
}

fn send_background_control_request(
    bind_addr: &str,
    token: &str,
    command: BackgroundWorkerCommand,
) -> Result<BackgroundStatusSnapshot> {
    let addr = bind_addr
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid background TCP address: {bind_addr}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .with_context(|| format!("failed to connect background TCP control {bind_addr}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("failed to set background TCP read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("failed to set background TCP write timeout")?;
    let request = BackgroundControlRequest {
        token: token.to_string(),
        command,
    };
    let text = serde_json::to_string(&request).context("failed to encode background request")?;
    writeln!(stream, "{text}").context("failed to write background request")?;
    stream
        .flush()
        .context("failed to flush background request")?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("failed to read background response")?;
    let response = serde_json::from_str::<BackgroundControlResponse>(&line)
        .context("failed to parse background response")?;
    if !response.ok {
        bail!(
            "background worker rejected request: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    response
        .status
        .context("background response missing status")
}

fn send_background_control_request_to_state(
    state: &BackgroundTaskState,
    command: BackgroundWorkerCommand,
) -> Result<BackgroundStatusSnapshot> {
    send_background_control_request(&state.bind_addr, &state.token, command)
}

fn spawn_background_status_poll(target: BackgroundStatusTarget) -> BackgroundStatusPollJob {
    let worker_target = target.clone();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = send_background_control_request(
            &worker_target.bind_addr,
            &worker_target.token,
            BackgroundWorkerCommand::Status,
        )
        .map_err(|error| format!("{error:#}"));
        // A successful control response already proves that the process is alive. Only use the
        // slower platform process lookup after a TCP failure, and keep it off the TUI thread.
        let process_alive = result.is_ok() || process_exists(worker_target.pid);
        let _ = tx.send(BackgroundStatusPollOutcome {
            result,
            process_alive,
        });
    });
    BackgroundStatusPollJob {
        target,
        receiver: rx,
        worker,
    }
}

fn wait_for_background_registry(child: &mut Child, log_path: &Path) -> Result<BackgroundTaskState> {
    let pid = child.id();
    let state_path = background_task_state_path();
    let deadline = Instant::now() + BACKGROUND_REGISTRY_WAIT_TIMEOUT;
    while Instant::now() < deadline {
        match read_background_task_state()? {
            Some(state)
                if state.pid == pid && !state.bind_addr.is_empty() && !state.token.is_empty() =>
            {
                return Ok(state);
            }
            Some(_) | None => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                bail!(
                    "background worker process {pid} exited with {status} before publishing TCP registry {}{}",
                    state_path.display(),
                    background_log_tail_context(log_path)
                );
            }
            Ok(None) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to query background worker process {pid} status")
                });
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    let still_running = child.try_wait().ok().flatten().is_none();
    bail!(
        "timed out waiting for background worker process {pid} to publish TCP registry {} (still_running={still_running}){}",
        state_path.display(),
        background_log_tail_context(log_path)
    )
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
        app.poll_private_access_updates()?;
        app.poll_verify_updates();
        app.poll_background_auto_pick_status()?;
        app.maybe_start_subscription_refresh();
        app.maybe_refresh_latency_chart()?;
        app.maybe_refresh_connections();
        app.maybe_refresh_system_proxy_status();
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if matches!(key.code, KeyCode::Char('V'))
                    && app.private_access_connect_needs_terminal_prompt()
                {
                    connect_private_access_with_terminal_prompt(&mut terminal, app)?;
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

fn suspend_terminal_for_prompt(terminal: &mut DefaultTerminal) -> Result<()> {
    terminal.show_cursor()?;
    restore_terminal()?;
    println!();
    println!("Private Access TUN helper may ask for your macOS sudo password below.");
    println!(
        "Complete the prompt in this terminal; the TUI will return after the helper is ready or fails."
    );
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

fn connect_private_access_with_terminal_prompt(
    terminal: &mut DefaultTerminal,
    app: &mut App,
) -> Result<()> {
    app.open_private_access_progress();
    app.push_private_access_progress(
        PrivateAccessProgressTone::Info,
        "正在连接内网服务器...".to_string(),
    );
    app.push_private_access_progress(
        PrivateAccessProgressTone::Info,
        "需要管理员权限创建 TUN 接口，正在切换到终端提示...".to_string(),
    );
    terminal.draw(|frame| draw(frame, app))?;
    suspend_terminal_for_prompt(terminal)?;
    let result = (|| -> Result<()> {
        app.toggle_private_access()?;
        loop {
            app.poll_private_access_updates()?;
            let state = app.private_access.focused().state.clone();
            if !matches!(state, PrivateAccessState::Connecting) {
                println!();
                println!("Private Access {}: {}", state.label(), app.status);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    })();
    let resume_result = resume_terminal_after_prompt(terminal);
    result.and(resume_result)
}

fn draw(frame: &mut Frame, app: &mut App) {
    let implicit_root_mode = app.implicit_root_mode();
    let status_lines = status_lines(app);
    let status_line_count = status_lines.len() as u16;
    let status_height = status_line_count.saturating_add(2).max(3);
    let [main, status_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(status_height)])
            .areas(frame.area());
    let [groups_area, members_area] =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).areas(main);
    let (internet_area, intranet_area) = if app.private_access.is_configured() {
        let [internet_area, intranet_area] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(groups_area);
        (internet_area, Some(intranet_area))
    } else {
        (groups_area, None)
    };

    let groups = app
        .displayed_group_names()
        .iter()
        .map(|group_name| {
            let group = app.group_by_name(group_name);
            let current = group
                .and_then(|group| group.current.as_deref())
                .map_or(String::from("unset"), ToString::to_string);
            let is_current = app
                .implicit_root_group()
                .and_then(|root| root.current.as_deref())
                == Some(group_name.as_str());
            let mut style = Style::default().fg(Color::Cyan);
            if implicit_root_mode && is_current {
                style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
            }
            ListItem::new(Line::from(vec![
                Span::styled(
                    truncate_for_width(group_name, internet_area.width.saturating_sub(18) as usize),
                    style,
                ),
                Span::raw(" "),
                Span::styled(
                    format!("[{}]", truncate_for_width(&current, 14)),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(if implicit_root_mode && is_current {
                    "  *"
                } else {
                    ""
                }),
            ]))
        })
        .collect::<Vec<_>>();

    let groups_title = "Internet Proxy";
    let groups_block = Block::default()
        .title(groups_title)
        .borders(Borders::ALL)
        .border_style(border_style(
            app.focus == Focus::Groups && app.left_pane_section == LeftPaneSection::Internet,
        ));
    let groups_widget = List::new(groups)
        .block(groups_block)
        .highlight_style(selected_style(
            app.focus == Focus::Groups && app.left_pane_section == LeftPaneSection::Internet,
        ))
        .highlight_symbol("> ");
    let mut groups_state = ListState::default().with_selected(
        (app.left_pane_section == LeftPaneSection::Internet).then_some(app.displayed_group_index()),
    );
    frame.render_stateful_widget(groups_widget, internet_area, &mut groups_state);

    if let Some(intranet_area) = intranet_area {
        let profiles = app
            .private_access
            .profiles
            .iter()
            .map(|profile| {
                let state_label = if profile.background_pid.is_some() {
                    "BACKGROUND"
                } else {
                    private_access_state_badge(profile.state.clone())
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        truncate_for_width(
                            &profile.id,
                            intranet_area.width.saturating_sub(18) as usize,
                        ),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw("  "),
                    Span::styled(state_label, private_access_state_style(&profile.state)),
                ]))
            })
            .collect::<Vec<_>>();
        let intranet_active =
            app.focus == Focus::Groups && app.left_pane_section == LeftPaneSection::Intranet;
        let intranet_block = Block::default()
            .title("Intranet Proxy")
            .borders(Borders::ALL)
            .border_style(border_style(intranet_active));
        let intranet_widget = List::new(profiles)
            .block(intranet_block)
            .highlight_style(selected_style(intranet_active))
            .highlight_symbol("> ");
        let mut intranet_state = ListState::default().with_selected(
            (app.left_pane_section == LeftPaneSection::Intranet)
                .then_some(app.private_access.focused_index),
        );
        frame.render_stateful_widget(intranet_widget, intranet_area, &mut intranet_state);
    }

    let displayed_members = app.displayed_members();
    let members = app
        .selected_member_panel_group()
        .map(|group| {
            displayed_members
                .iter()
                .map(|member| {
                    let is_current = group.current.as_deref() == Some(member.as_str());
                    let bench = app
                        .selected_benchmark()
                        .and_then(|summary| summary.find_result(member));
                    let mut style = Style::default();
                    if is_current {
                        style = style.fg(Color::Green).add_modifier(Modifier::BOLD);
                    }
                    let (marker, marker_style, loading_suffix) = match bench {
                        Some(result) if !result.completed => (
                            result.display_delay(),
                            Style::default()
                                .fg(Color::LightYellow)
                                .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
                            "  ⟳",
                        ),
                        Some(result) if result.delay.is_some() => (
                            result.display_delay(),
                            Style::default().fg(Color::Magenta),
                            "",
                        ),
                        Some(result) => (
                            result.display_delay(),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                            "",
                        ),
                        None => ("-".to_string(), Style::default().fg(Color::DarkGray), ""),
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            truncate_for_width(
                                member,
                                members_area.width.saturating_sub(16) as usize,
                            ),
                            style,
                        ),
                        Span::raw("  "),
                        Span::styled(marker, marker_style),
                        Span::raw(loading_suffix),
                        Span::raw(if is_current { "  *" } else { "" }),
                    ]))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let members_title = app
        .selected_member_panel_group()
        .map(|group| {
            format!(
                "Candidates for {} [{}]",
                group.name,
                node_order_badge(app.latency_sort_mode)
            )
        })
        .unwrap_or_else(|| format!("Candidates [{}]", node_order_badge(app.latency_sort_mode)));
    let members_block = Block::default()
        .title(members_title)
        .borders(Borders::ALL)
        .border_style(border_style(app.focus == Focus::Members));
    let members_widget = List::new(members)
        .block(members_block)
        .highlight_style(selected_style(app.focus == Focus::Members))
        .highlight_symbol("> ");
    let mut members_state = ListState::default().with_selected(app.displayed_member_index());
    frame.render_stateful_widget(members_widget, members_area, &mut members_state);

    if app.showing_intranet_details()
        && let Some(profile) = app.private_access.focused_opt()
    {
        frame.render_widget(Clear, members_area);
        let detail_view = app.intranet_detail_view(profile);
        let details_block = Block::default()
            .title(if app.intranet_detail_scroll == 0 {
                format!("Intranet: {}", profile.id)
            } else {
                format!(
                    "Intranet: {} [line {}]",
                    profile.id,
                    app.intranet_detail_scroll + 1
                )
            })
            .borders(Borders::ALL)
            .border_style(border_style(app.focus == Focus::Members));
        let details_inner = details_block.inner(members_area);
        frame.render_widget(details_block, members_area);
        let [details_area, footer_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(details_inner);
        let details = Paragraph::new(detail_view.lines)
            .wrap(Wrap { trim: false })
            .scroll((app.intranet_detail_scroll, 0));
        frame.render_widget(details, details_area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "j/k scroll  Enter expand/fold  V connect/disconnect  o configure",
                Style::default().fg(Color::DarkGray),
            ))),
            footer_area,
        );
    }

    let help =
        Paragraph::new(status_lines).block(Block::default().title("Status").borders(Borders::ALL));
    frame.render_widget(help, status_area);

    if let Some(message) = app.flash_message() {
        let height = (message.lines().count() as u16).saturating_add(2).max(7);
        let area = centered_rect(80, height, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(message).block(Block::default().title("Info").borders(Borders::ALL)),
            area,
        );
    }
    if let Some(chart) = app.latency_chart.as_ref() {
        draw_latency_chart(frame, chart);
    }
    if app.show_connections {
        draw_connections_panel(frame, app);
    }
    if app.show_help {
        draw_help_panel(frame, app);
    }
    if app.show_settings {
        draw_settings_panel(frame, app);
    }
    if app.onboarding.is_some() {
        draw_onboarding_panel(frame, app);
    }
    if let Some(progress) = app.private_access_progress.as_ref() {
        draw_private_access_progress_panel(frame, progress);
    }
    if let Some(auth) = app.private_access_auth.as_ref() {
        draw_private_access_auth_panel(frame, auth);
    }
    if let Some(input) = app.filter_input.as_deref() {
        let cursor_x = status_area
            .x
            .saturating_add(1)
            .saturating_add(unicode_width::UnicodeWidthStr::width("Filter: ") as u16)
            .saturating_add(unicode_width::UnicodeWidthStr::width(input) as u16);
        let cursor_y = status_area.y.saturating_add(status_line_count);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn private_access_detail_view(
    profile: &PrivateAccessProfileRuntime,
    is_expanded: impl Fn(IntranetDetailSection) -> bool,
) -> IntranetDetailView {
    let state_label = if profile.background_pid.is_some() {
        "BACKGROUND"
    } else {
        private_access_state_badge(profile.state.clone())
    };
    let gateway = if profile.server.trim().is_empty() {
        "not configured".to_string()
    } else {
        format!("{}:{}", profile.server, profile.port)
    };
    let data_plane = match profile.mode {
        PrivateAccessMode::Tun => "TUN".to_string(),
        PrivateAccessMode::Bridge => profile
            .bridge
            .as_ref()
            .map(|bridge| format!("{} at {}", bridge.kind, bridge.listen))
            .unwrap_or_else(|| format!("HTTP bridge at {}", profile.bridge_listen)),
    };
    let capabilities = [
        profile
            .manifest
            .capabilities
            .pushed_routes
            .then_some("routes"),
        profile.manifest.capabilities.pushed_dns.then_some("DNS"),
        profile
            .manifest
            .capabilities
            .local_http_bridge
            .then_some("HTTP bridge"),
        profile
            .manifest
            .capabilities
            .graceful_disconnect
            .then_some("graceful disconnect"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");

    let mut lines = vec![
        Line::from(vec![
            Span::styled("State: ", Style::default().fg(Color::DarkGray)),
            Span::styled(state_label, private_access_state_style(&profile.state)),
        ]),
        private_access_detail_line("Service", &profile.manifest.name),
        private_access_detail_line(
            "Protocol",
            &format!(
                "{} (service v{})",
                profile.manifest.protocol, profile.manifest.version
            ),
        ),
        private_access_detail_line("Gateway", &gateway),
        private_access_detail_line(
            "TLS verification",
            if profile.tls_verify {
                "enabled"
            } else {
                "disabled"
            },
        ),
        private_access_detail_line("Data plane", &data_plane),
    ];
    if !capabilities.is_empty() {
        lines.push(private_access_detail_line("Capabilities", &capabilities));
    }
    if let Some(pid) = profile.background_pid {
        lines.push(private_access_detail_line(
            "Process",
            &format!("background pid {pid}"),
        ));
    } else if profile.process.is_some() {
        lines.push(private_access_detail_line("Process", "owned by this TUI"));
    }

    let mut sections = Vec::new();
    append_private_access_detail_section(
        &mut lines,
        &mut sections,
        IntranetDetailSection::Dns,
        "DNS servers",
        profile.dns.clone(),
        "No DNS servers have been pushed.",
        is_expanded(IntranetDetailSection::Dns),
    );
    append_private_access_detail_section(
        &mut lines,
        &mut sections,
        IntranetDetailSection::Routes,
        "Routes",
        profile
            .routes
            .iter()
            .map(|route| route.cidr.clone())
            .collect(),
        "No routes have been pushed.",
        is_expanded(IntranetDetailSection::Routes),
    );
    let domains = profile
        .domains
        .iter()
        .map(|domain| format!("exact  {domain}"))
        .chain(
            profile
                .domain_suffixes
                .iter()
                .map(|domain| format!("suffix *.{domain}")),
        )
        .collect();
    append_private_access_detail_section(
        &mut lines,
        &mut sections,
        IntranetDetailSection::Domains,
        "Internal domains",
        domains,
        "No internal domains have been pushed.",
        is_expanded(IntranetDetailSection::Domains),
    );

    if let Some(error) = profile.last_error.as_deref() {
        lines.push(Line::default());
        lines.push(private_access_detail_heading("Last error"));
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().fg(Color::Red),
        )));
    }

    IntranetDetailView { lines, sections }
}

fn append_private_access_detail_section(
    lines: &mut Vec<Line<'static>>,
    sections: &mut Vec<IntranetDetailSectionRange>,
    section: IntranetDetailSection,
    label: &str,
    items: Vec<String>,
    empty_message: &str,
    expanded: bool,
) {
    const FOLDED_ITEM_LIMIT: usize = 10;

    lines.push(Line::default());
    let start = lines.len();
    let item_count = items.len();
    let foldable = item_count > FOLDED_ITEM_LIMIT;
    let visible_count = if foldable && !expanded {
        FOLDED_ITEM_LIMIT
    } else {
        item_count
    };
    let heading = match (foldable, expanded) {
        (true, true) => format!("▼ {label} ({item_count}) [Enter to fold]"),
        (true, false) => {
            format!("▶ {label} ({item_count}) [showing {FOLDED_ITEM_LIMIT}; Enter to expand]")
        }
        (false, _) => format!("{label} ({item_count})"),
    };
    lines.push(private_access_detail_heading(heading));
    if items.is_empty() {
        lines.push(private_access_detail_empty(empty_message));
    } else {
        lines.extend(
            items
                .into_iter()
                .take(visible_count)
                .map(|item| Line::from(format!("  {item}"))),
        );
        if foldable && !expanded {
            lines.push(Line::from(Span::styled(
                format!("  … {} more item(s)", item_count - visible_count),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    sections.push(IntranetDetailSectionRange {
        section,
        start,
        end: lines.len(),
        foldable,
    });
}

fn private_access_detail_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

fn private_access_detail_heading(value: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        value.into(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn private_access_detail_empty(value: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {value}"),
        Style::default().fg(Color::DarkGray),
    ))
}

fn status_lines(app: &App) -> Vec<Line<'_>> {
    let benchmark_hint = if app.showing_intranet_details() {
        "Intranet details are shown in the right panel".to_string()
    } else {
        app.selected_benchmark().map_or_else(
            || {
                format!(
                    "clash={}  order={}  auto={}  T group latency  t node latency  a auto-pick  / filter",
                    app.clash_mode_label(),
                    node_order_badge(app.latency_sort_mode),
                    auto_select_badge(app.auto_select_enabled)
                )
            },
            |summary| {
                let best = summary
                    .best_success()
                    .map(|item| format!("best={} {}", item.name, item.display_delay()))
                    .unwrap_or_else(|| "best=none".to_string());
                format!(
                    "filter='{}'  tested={}  clash={}  order={}  auto={}  {}",
                    summary.pattern,
                    summary.results.len(),
                    app.clash_mode_label(),
                    node_order_badge(app.latency_sort_mode),
                    auto_select_badge(app.auto_select_enabled),
                    truncate_for_width(&best, 30)
                )
            },
        )
    };

    let bottom_line = if let Some(input) = app.filter_input.as_deref() {
        Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(Color::Cyan)),
            Span::raw(input),
            Span::styled(
                "  Enter apply  Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if let Some(input) = app.bypass_input.as_deref() {
        Line::from(vec![
            Span::styled("Bypass: ", Style::default().fg(Color::Cyan)),
            Span::raw(input),
            Span::styled(
                "  domains/IPs/CIDRs comma-separated  Enter save  Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(app.status_line())
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Arrows/jk", Style::default().fg(Color::Cyan)),
            Span::raw(" move  "),
            Span::styled("Tab/h/l", Style::default().fg(Color::Cyan)),
            Span::raw(" switch pane  "),
            Span::styled("Space", Style::default().fg(Color::Cyan)),
            Span::raw(" select  "),
            Span::styled("m", Style::default().fg(Color::Cyan)),
            Span::raw(" clash mode  "),
            Span::styled("b", Style::default().fg(Color::Cyan)),
            Span::raw(" bypass  "),
            Span::styled("B", Style::default().fg(Color::Cyan)),
            Span::raw(" background  "),
            Span::styled("p", Style::default().fg(Color::Cyan)),
            Span::styled(
                " system proxy  ",
                Style::default().fg(if app.system_proxy_enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled("T/t", Style::default().fg(Color::Cyan)),
            Span::raw(" latency  "),
            Span::styled("s", Style::default().fg(Color::Cyan)),
            Span::raw(" sort order  "),
            Span::styled("a", Style::default().fg(Color::Cyan)),
            Span::raw(" auto-pick  "),
            Span::styled("i", Style::default().fg(Color::Cyan)),
            Span::raw(" info  "),
            Span::styled("c", Style::default().fg(Color::Cyan)),
            Span::raw(" connections  "),
            Span::styled("v", Style::default().fg(Color::Cyan)),
            Span::raw(" verify  "),
            Span::styled("V", Style::default().fg(Color::Cyan)),
            Span::raw(" private access  "),
            Span::styled("o", Style::default().fg(Color::Cyan)),
            Span::raw(" settings  "),
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::raw(" filter  "),
            Span::styled("r", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh  "),
            Span::styled("u", Style::default().fg(Color::Cyan)),
            Span::raw(" update subs  "),
            Span::styled("?", Style::default().fg(Color::Cyan)),
            Span::raw(" help  "),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(" quit"),
        ]),
        Line::from(vec![
            Span::styled("Controller: ", Style::default().fg(Color::DarkGray)),
            Span::raw(app.client.base_url.as_str()),
        ]),
        Line::from(benchmark_hint),
        Line::from(app.connections_summary_line()),
        Line::from(app.subscription_summary_line()),
        Line::from(app.sing_box_summary_line()),
    ];
    lines.push(bottom_line);
    lines
}

#[derive(Clone, Copy)]
struct HelpBinding {
    key: &'static str,
    summary: &'static str,
    detail: &'static str,
}

const HELP_BINDINGS: &[HelpBinding] = &[
    HelpBinding {
        key: "up",
        summary: "Move up / scroll details",
        detail: "Move the highlighted row up, or scroll Intranet Proxy details when the right pane is focused.",
    },
    HelpBinding {
        key: "k",
        summary: "Move selection up",
        detail: "Vim-style shortcut for moving the highlighted row up.",
    },
    HelpBinding {
        key: "down",
        summary: "Move down / scroll details",
        detail: "Move the highlighted row down, crossing from Internet Proxy into Intranet Proxy, or scroll right-pane details.",
    },
    HelpBinding {
        key: "j",
        summary: "Move selection down",
        detail: "Vim-style shortcut for moving the highlighted row down.",
    },
    HelpBinding {
        key: "tab",
        summary: "Switch pane",
        detail: "Move focus between the selector/group pane and the candidate node pane.",
    },
    HelpBinding {
        key: "h",
        summary: "Switch to left pane",
        detail: "Move focus to the pane on the left.",
    },
    HelpBinding {
        key: "l",
        summary: "Switch to right pane",
        detail: "Move focus to the pane on the right.",
    },
    HelpBinding {
        key: "g",
        summary: "Move to first item",
        detail: "Jump to the first item in the focused list.",
    },
    HelpBinding {
        key: "G",
        summary: "Move to last item",
        detail: "Jump to the last item in the focused list.",
    },
    HelpBinding {
        key: "space",
        summary: "Activate selection",
        detail: "Apply an Internet Proxy selection, or open the selected Intranet Proxy profile details.",
    },
    HelpBinding {
        key: "m",
        summary: "Cycle Clash mode",
        detail: "Switch the controller between available Clash modes.",
    },
    HelpBinding {
        key: "s",
        summary: "Toggle latency sort order",
        detail: "Toggle candidate display between selector order and successful latency order.",
    },
    HelpBinding {
        key: "T",
        summary: "Test current group latency",
        detail: "Start an asynchronous latency test for all visible candidates in the selected group.",
    },
    HelpBinding {
        key: "t",
        summary: "Test selected node latency",
        detail: "Start an asynchronous latency test for only the highlighted node.",
    },
    HelpBinding {
        key: "/",
        summary: "Edit latency filter",
        detail: "Open the filter editor. Comma-separated values include matches; prefix with ! or - to exclude.",
    },
    HelpBinding {
        key: "a",
        summary: "Toggle auto-pick",
        detail: "Enable or disable periodic latency tests and automatic switching for the current filter, or all nodes when the filter is empty.",
    },
    HelpBinding {
        key: "i",
        summary: "Open latency chart",
        detail: "Show SQLite-backed latency history for the highlighted node.",
    },
    HelpBinding {
        key: "z",
        summary: "Zoom latency chart in",
        detail: "When the latency chart is open, narrow the displayed time window.",
    },
    HelpBinding {
        key: "Z",
        summary: "Zoom latency chart out",
        detail: "When the latency chart is open, widen the displayed time window.",
    },
    HelpBinding {
        key: "c",
        summary: "Show active connections",
        detail: "Open a panel with active connection targets, outbound chains, and matched rules.",
    },
    HelpBinding {
        key: "b",
        summary: "Edit bypass rules",
        detail: "Edit direct-bypass domains, IPs, and CIDRs written to the local rule-set.",
    },
    HelpBinding {
        key: "B",
        summary: "Keep sing-box running",
        detail: "Exit the TUI while leaving sing-box, auto-pick, and active Private Access sessions running in the background.",
    },
    HelpBinding {
        key: "p",
        summary: "Toggle system proxy",
        detail: "Enable or disable the OS system proxy for the detected sing-box mixed inbound.",
    },
    HelpBinding {
        key: "u",
        summary: "Update subscriptions",
        detail: "Force a background subscription refresh when subscription refresh is configured.",
    },
    HelpBinding {
        key: "v",
        summary: "Verify network",
        detail: "Run configured connectivity checks in the background.",
    },
    HelpBinding {
        key: "V",
        summary: "Toggle Private Access",
        detail: "Connect or disconnect the selected Intranet Proxy profile.",
    },
    HelpBinding {
        key: "o",
        summary: "Open settings",
        detail: "Edit TUI latency, auto-pick, and system proxy settings.",
    },
    HelpBinding {
        key: "r",
        summary: "Refresh groups",
        detail: "Reload selector groups, mode, and connection state from the controller.",
    },
    HelpBinding {
        key: "?",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "esc",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "enter",
        summary: "Close help",
        detail: "Close this keybindings panel.",
    },
    HelpBinding {
        key: "q",
        summary: "Quit",
        detail: "Exit the TUI. When help is open, q still exits the application.",
    },
];

fn draw_help_panel(frame: &mut Frame, app: &App) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(108);
    let height = frame_area.height.saturating_sub(4).min(34);
    let area = centered_rect(width.max(56), height.max(18), frame_area);
    frame.render_widget(Clear, area);
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(3)]).areas(area);
    let selected = app.help_index.min(HELP_BINDINGS.len().saturating_sub(1));
    let visible_rows = list_area.height.saturating_sub(2).max(1) as usize;
    let first = selected.saturating_sub(visible_rows.saturating_sub(1));

    let lines = HELP_BINDINGS
        .iter()
        .enumerate()
        .skip(first)
        .take(visible_rows)
        .map(|(index, binding)| help_binding(*binding, index == selected))
        .collect::<Vec<_>>();

    let widget = Paragraph::new(lines).block(
        Block::default()
            .title("Keybindings")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    frame.render_widget(widget, list_area);

    let count = format!("{} of {}", selected + 1, HELP_BINDINGS.len());
    let count_width = unicode_width::UnicodeWidthStr::width(count.as_str()) as u16;
    let count_area = ratatui::layout::Rect {
        x: list_area.x + list_area.width.saturating_sub(count_width + 1),
        y: list_area.y + list_area.height.saturating_sub(1),
        width: count_width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(count).style(Style::default().fg(Color::Green)),
        count_area,
    );

    let detail = HELP_BINDINGS
        .get(selected)
        .map(|binding| binding.detail)
        .unwrap_or("");
    frame.render_widget(
        Paragraph::new(detail)
            .style(Style::default().fg(Color::Gray))
            .block(Block::default().borders(Borders::ALL)),
        detail_area,
    );
}

fn help_binding(binding: HelpBinding, selected: bool) -> Line<'static> {
    let line_style = if selected {
        Style::default().bg(Color::Blue)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:>7}", binding.key),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(binding.summary, Style::default().fg(Color::Gray)),
    ])
    .style(line_style)
}

fn draw_settings_panel(frame: &mut Frame, app: &App) {
    let frame_area = frame.area();
    let area = centered_rect(96, 26, frame_area);
    frame.render_widget(Clear, area);
    let fields = visible_settings_fields(app);
    let selected = app.settings_index.min(fields.len().saturating_sub(1));
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" edit  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" close"),
    ]));
    lines.push(Line::raw(""));
    for (index, field) in fields.iter().enumerate() {
        let marker = if index == selected { "> " } else { "  " };
        let style = if index == selected {
            Style::default().bg(Color::Blue)
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker),
                Span::styled(
                    settings_field_label(*field),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw("  "),
                Span::raw(settings_field_display_value(app, *field)),
            ])
            .style(style),
        );
    }
    if let Some(edit) = &app.settings_edit {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("Editing ", Style::default().fg(Color::Yellow)),
            Span::raw(settings_field_label(edit.field)),
            Span::raw(": "),
            Span::raw(edit.input.as_str()),
        ]));
    }
    let settings_error = app
        .settings_edit
        .as_ref()
        .and_then(|edit| edit.error.as_deref())
        .or(app.settings_error.as_deref());
    if let Some(error) = settings_error {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                "Error: ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_for_width(error, area.width.saturating_sub(12) as usize),
                Style::default().fg(Color::Red),
            ),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title("Settings")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        ),
        area,
    );
}

fn visible_settings_fields(app: &App) -> Vec<SettingsField> {
    SETTINGS_FIELDS
        .iter()
        .copied()
        .filter(|field| {
            !is_private_access_settings_field(*field) || app.private_access.is_configured()
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
            | SettingsField::PrivateAccessTlsVerify
    )
}

fn draw_onboarding_panel(frame: &mut Frame, app: &App) {
    let frame_area = frame.area();
    let area = centered_rect(86, 13, frame_area);
    frame.render_widget(Clear, area);
    let Some(onboarding) = &app.onboarding else {
        return;
    };
    let lines = vec![
        Line::from("First run setup"),
        Line::raw(""),
        Line::from("Paste one sing-box subscription URL and press Enter to save it to .suburl."),
        Line::from("Press s to skip, or Esc to keep this wizard for next time."),
        Line::raw(""),
        Line::from(vec![
            Span::styled("URL: ", Style::default().fg(Color::Cyan)),
            Span::raw(onboarding.input.as_str()),
        ]),
        Line::raw(""),
        Line::from(onboarding.message.as_str()),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title("Welcome")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        ),
        area,
    );
}

fn draw_private_access_progress_panel(frame: &mut Frame, progress: &PrivateAccessProgressModal) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).min(88).max(56);
    let height = (progress.entries.len() as u16 + 4)
        .min(frame_area.height.saturating_sub(4))
        .max(8);
    let area = centered_rect(width, height, frame_area);
    frame.render_widget(Clear, area);

    let max_entries = area.height.saturating_sub(4) as usize;
    let start = progress.entries.len().saturating_sub(max_entries);
    let mut lines = progress
        .entries
        .iter()
        .skip(start)
        .map(|entry| {
            Line::from(vec![
                Span::styled(entry.tone.prefix(), entry.tone.style()),
                Span::raw(truncate_for_width(
                    &entry.text,
                    area.width.saturating_sub(8) as usize,
                )),
            ])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if progress.done {
            "Enter/Esc close"
        } else {
            "Private Access is running..."
        },
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(progress.title.clone())
                .borders(Borders::ALL)
                .border_style(if progress.done {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Cyan)
                }),
        ),
        area,
    );
}

fn draw_private_access_auth_panel(frame: &mut Frame, auth: &PrivateAccessAuthModal) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(6).min(82).max(52);
    let message_rows = usize::from(!auth.message.trim().is_empty());
    let height = (auth.fields.len() + message_rows + 6) as u16;
    let area = centered_rect(
        width,
        height.min(frame_area.height.saturating_sub(4)).max(9),
        frame_area,
    );
    frame.render_widget(Clear, area);

    let mut lines = vec![Line::from(vec![
        Span::styled("Enter", Style::default().fg(Color::Cyan)),
        Span::raw(" next/submit  "),
        Span::styled("Esc", Style::default().fg(Color::Cyan)),
        Span::raw(" cancel"),
    ])];
    if !auth.message.trim().is_empty() {
        lines.push(Line::from(auth.message.as_str()));
    }
    lines.push(Line::raw(""));
    let field_start = lines.len();
    for (index, field) in auth.fields.iter().enumerate() {
        let selected = index == auth.field_index;
        let marker = if selected { "> " } else { "  " };
        let value = private_access_auth_display_value(field, &auth.inputs[index]);
        let style = if selected {
            Style::default().bg(Color::Blue)
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker),
                Span::styled(field.label.as_str(), Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::raw(value),
            ])
            .style(style),
        );
    }
    if let Some(error) = auth.error.as_deref() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(error, Style::default().fg(Color::Red)));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(auth.title.as_str())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        ),
        area,
    );

    if let Some(field) = auth.fields.get(auth.field_index) {
        let display = private_access_auth_display_value(field, &auth.inputs[auth.field_index]);
        let cursor_x = area
            .x
            .saturating_add(1)
            .saturating_add(2)
            .saturating_add(unicode_width::UnicodeWidthStr::width(field.label.as_str()) as u16)
            .saturating_add(2)
            .saturating_add(unicode_width::UnicodeWidthStr::width(display.as_str()) as u16)
            .min(area.x.saturating_add(area.width.saturating_sub(2)));
        let cursor_y = area
            .y
            .saturating_add(1)
            .saturating_add(field_start as u16)
            .saturating_add(auth.field_index as u16)
            .min(area.y.saturating_add(area.height.saturating_sub(2)));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn private_access_auth_display_value(_field: &PrivateAccessAuthField, input: &str) -> String {
    input.to_string()
}

fn private_access_auth_initial_value(
    profile: &PrivateAccessProfileRuntime,
    field: &PrivateAccessAuthField,
) -> String {
    if let Some(option) = field.options.first() {
        return option.value.clone();
    }
    if profile.manifest.id != "sonicwall" {
        return String::new();
    }

    let has_kind_marker = |expected: &str| {
        field
            .kind
            .split(|character: char| {
                character.is_ascii_whitespace() || matches!(character, ',' | ';')
            })
            .any(|marker| marker.eq_ignore_ascii_case(expected))
    };
    if has_kind_marker("is-username") {
        return profile.username.clone();
    }
    if has_kind_marker("is-password") {
        if !profile.password.is_empty() {
            return profile.password.clone();
        }
        if !profile.password_env.is_empty() {
            return env::var(&profile.password_env).unwrap_or_default();
        }
    }

    // A generic sensitive/password field may be an OTP or another dynamic reply.
    // Only the gateway's explicit is-password marker is safe to prefill.
    String::new()
}

fn settings_field_label(field: SettingsField) -> &'static str {
    match field {
        SettingsField::BenchmarkUrl => "Latency URL",
        SettingsField::BenchmarkTimeoutMs => "Latency timeout ms",
        SettingsField::RequestTimeoutSec => "Request timeout sec",
        SettingsField::MaxConcurrency => "Max concurrency",
        SettingsField::VerifyTargets => "Verification targets",
        SettingsField::AutoPickThresholdMs => "Auto-pick threshold ms",
        SettingsField::AutoPickIntervalSec => "Auto-pick interval sec",
        SettingsField::SystemProxyServer => "System proxy server",
        SettingsField::PrivateAccessProfile => "Private Access profile",
        SettingsField::PrivateAccessManifestPath => "Private Access service manifest",
        SettingsField::PrivateAccessMode => "Private Access mode",
        SettingsField::PrivateAccessServer => "Private Access server",
        SettingsField::PrivateAccessPort => "Private Access port",
        SettingsField::PrivateAccessUsername => "Private Access username",
        SettingsField::PrivateAccessPassword => "Private Access password",
        SettingsField::PrivateAccessPasswordEnv => "Private Access password env",
        SettingsField::PrivateAccessBridgeListen => "Private Access bridge listen",
        SettingsField::PrivateAccessTlsVerify => "Private Access TLS verify",
    }
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
        SettingsField::SystemProxyServer => app.system_proxy_server.clone(),
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

fn draw_connections_panel(frame: &mut Frame, app: &App) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(4).min(120);
    let height = frame_area.height.saturating_sub(4).min(24);
    let area = centered_rect(width.max(20), height.max(8), frame_area);
    frame.render_widget(Clear, area);

    let inner_width = area.width.saturating_sub(4) as usize;
    let max_rows = area.height.saturating_sub(6) as usize;
    let mut lines = vec![
        Line::from(app.connections_summary_line()),
        Line::from(vec![
            Span::styled("Source", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Target", Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Chain", Style::default().fg(Color::Cyan)),
        ]),
    ];

    if let Some(error) = &app.connection_error {
        lines.push(Line::from(format!(
            "error: {}",
            truncate_for_width(error, inner_width.saturating_sub(7))
        )));
    } else if app.connections.connections.is_empty() {
        lines.push(Line::from("No active connections"));
    } else {
        lines.extend(
            app.connections
                .connections
                .iter()
                .take(max_rows)
                .map(|connection| Line::from(format_connection_line(connection, inner_width))),
        );
        let hidden = app.connections.connections.len().saturating_sub(max_rows);
        if hidden > 0 {
            lines.push(Line::from(format!("... {hidden} more connections")));
        }
    }

    let widget = Paragraph::new(lines).block(
        Block::default()
            .title("Active Connections (c/Esc close, r refresh)")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(widget, area);
}

fn draw_latency_chart(frame: &mut Frame, chart: &LatencyChartState) {
    let area = centered_rect(90, 20, frame.area());
    frame.render_widget(Clear, area);

    let visible_samples = latency_chart_windowed_samples(&chart.samples, chart.window);
    let segments = latency_chart_segments(&visible_samples);
    let Some(start_ms) = latency_chart_window_start_ms(&chart.samples, chart.window) else {
        frame.render_widget(
            Paragraph::new("No latency history")
                .block(Block::default().title("Latency").borders(Borders::ALL)),
            area,
        );
        return;
    };
    let time_unit = latency_chart_time_unit(chart.window);
    let scale = match time_unit {
        LatencyChartTimeUnit::Minutes => 60_000.0,
        LatencyChartTimeUnit::Hours => 3_600_000.0,
    };
    let segment_data = segments
        .iter()
        .map(|segment| {
            segment
                .iter()
                .map(|point| {
                    (
                        point.0.saturating_sub(start_ms) as f64 / scale,
                        point.1 as f64,
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    if segment_data.iter().all(Vec::is_empty) {
        frame.render_widget(
            Paragraph::new("No successful latency samples in this window")
                .block(Block::default().title("Latency").borders(Borders::ALL)),
            area,
        );
        return;
    }

    let (min_y, max_y) = segment_data
        .iter()
        .flatten()
        .fold((f64::MAX, f64::MIN), |(min_y, max_y), (_, y)| {
            (min_y.min(*y), max_y.max(*y))
        });
    let x_max = chart.window.as_millis() as f64 / scale;
    let x_bounds = [0.0, x_max.max(1.0)];
    let y_bounds = latency_chart_y_bounds(min_y, max_y, AUTO_SELECT_THRESHOLD_MS);
    let title = format!(
        "Latency: {} / {} ({} samples, window {}, z/Z zoom)",
        chart.selector,
        truncate_for_width(&chart.node, 36),
        visible_samples.len(),
        latency_chart_window_label(chart.window)
    );
    let mut datasets = segment_data
        .iter()
        .enumerate()
        .map(|(index, data)| {
            Dataset::default()
                .name(format!("latency-{index}"))
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Magenta))
                .data(data)
        })
        .collect::<Vec<_>>();
    let threshold_data = latency_chart_threshold_line(x_bounds[1], AUTO_SELECT_THRESHOLD_MS);
    datasets.push(
        Dataset::default()
            .name(format!("{AUTO_SELECT_THRESHOLD_MS}ms limit"))
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Yellow))
            .data(&threshold_data),
    );
    let chart_widget = Chart::new(datasets)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .x_axis(
            Axis::default()
                .title(match time_unit {
                    LatencyChartTimeUnit::Minutes => "time (minutes)",
                    LatencyChartTimeUnit::Hours => "time (hours)",
                })
                .style(Style::default().fg(Color::Gray))
                .bounds(x_bounds)
                .labels(vec![
                    Span::raw(format!("{} ago", latency_chart_window_label(chart.window))),
                    Span::raw("now"),
                ]),
        )
        .y_axis(
            Axis::default()
                .title("latency (ms)")
                .style(Style::default().fg(Color::Gray))
                .bounds(y_bounds)
                .labels(vec![
                    Span::raw(format!("{:.0}", y_bounds[0])),
                    Span::raw(format!("{:.0}", y_bounds[1])),
                ]),
        );
    frame.render_widget(chart_widget, area);
}

fn latency_chart_segments(samples: &[NodeLatencySample]) -> Vec<Vec<(u64, u64)>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for sample in samples {
        if let Some(delay_ms) = sample.delay_ms {
            current.push((sample.recorded_at_ms, delay_ms));
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn latency_chart_threshold_line(x_max: f64, threshold_ms: u64) -> Vec<(f64, f64)> {
    vec![
        (0.0, threshold_ms as f64),
        (x_max.max(1.0), threshold_ms as f64),
    ]
}

fn latency_chart_y_bounds(min_y: f64, max_y: f64, threshold_ms: u64) -> [f64; 2] {
    let min_y = min_y.min(threshold_ms as f64);
    let max_y = max_y.max(threshold_ms as f64);
    let padding = ((max_y - min_y) * 0.05).max(10.0);
    [0.0_f64.max(min_y - padding), max_y + padding]
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AutoSelectSwitchPlan {
    target_node: Option<String>,
    parent_switch: Option<(String, String)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LatencyChartTimeUnit {
    Minutes,
    Hours,
}

fn latency_chart_time_unit(window: Duration) -> LatencyChartTimeUnit {
    if window >= Duration::from_secs(2 * 60 * 60) {
        LatencyChartTimeUnit::Hours
    } else {
        LatencyChartTimeUnit::Minutes
    }
}

fn latency_chart_window_label(window: Duration) -> String {
    if window >= Duration::from_secs(60 * 60) {
        format!("{}h", window.as_secs() / 3600)
    } else {
        format!("{}m", window.as_secs() / 60)
    }
}

fn latency_chart_zoom_in(window: Duration) -> Duration {
    (window / 2).max(LATENCY_CHART_MIN_WINDOW)
}

fn latency_chart_zoom_out(window: Duration) -> Duration {
    (window * 2).min(LATENCY_CHART_MAX_WINDOW)
}

fn latency_chart_latest_ms(samples: &[NodeLatencySample]) -> Option<u64> {
    samples.iter().map(|sample| sample.recorded_at_ms).max()
}

fn latency_chart_window_start_ms(samples: &[NodeLatencySample], window: Duration) -> Option<u64> {
    let latest = latency_chart_latest_ms(samples)?;
    Some(latest.saturating_sub(window.as_millis() as u64))
}

fn latency_chart_windowed_samples(
    samples: &[NodeLatencySample],
    window: Duration,
) -> Vec<NodeLatencySample> {
    let Some(start) = latency_chart_window_start_ms(samples, window) else {
        return Vec::new();
    };
    samples
        .iter()
        .filter(|sample| sample.recorded_at_ms >= start)
        .cloned()
        .collect()
}

fn border_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn selected_style(active: bool) -> Style {
    if active {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

fn node_order_badge(latency_sort_mode: bool) -> &'static str {
    if latency_sort_mode {
        "LATENCY ORDER"
    } else {
        "SELECTOR ORDER"
    }
}

fn auto_select_badge(auto_select_enabled: bool) -> &'static str {
    if auto_select_enabled { "ON" } else { "OFF" }
}

fn background_status_should_publish(status: &str) -> bool {
    status.starts_with("Auto-pick") || status.starts_with("Testing latency")
}

fn background_status_requires_selector_refresh(status: &str) -> bool {
    status.starts_with("Auto-pick switched") || status.starts_with("Auto-pick selected")
}

fn format_connection_line(connection: &ConnectionInfo, max_width: usize) -> String {
    let source = format_connection_source(connection);
    let target = format_connection_target(connection);
    let chain = if connection.chains.is_empty() {
        "-".to_string()
    } else {
        connection.chains.join(" -> ")
    };
    let rule = connection.rule.as_deref().unwrap_or("-");
    truncate_for_width(
        &format!("{source:<14} {target:<28} {chain}  {rule}"),
        max_width,
    )
}

fn format_connection_source(connection: &ConnectionInfo) -> String {
    let kind = connection.metadata.kind.as_deref().unwrap_or("-");
    let network = connection.metadata.network.as_deref().unwrap_or("-");
    format!("{kind}/{network}")
}

fn format_connection_target(connection: &ConnectionInfo) -> String {
    let target = connection
        .metadata
        .host
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(connection.metadata.destination_ip.as_deref())
        .unwrap_or("-");
    match connection.metadata.destination_port.as_deref() {
        Some(port) if !port.is_empty() => format!("{target}:{port}"),
        _ => target.to_string(),
    }
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

fn format_bytes_opt(bytes: Option<u64>) -> String {
    bytes.map(format_bytes).unwrap_or_else(|| "-".to_string())
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit_index = 0;
    while value >= 1024.0 && unit_index + 1 < UNITS.len() {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit_index])
    }
}

fn subscription_report_badge(report: &SubscriptionRefreshOutput) -> String {
    report
        .providers
        .iter()
        .map(|provider| {
            let warning = provider
                .warning
                .as_ref()
                .map(|warning| format!(" {}", truncate_for_width(warning, 24)))
                .unwrap_or_default();
            format!(
                "{}:{}:{} nodes{}",
                provider.provider, provider.status, provider.imported_nodes, warning
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_duration_badge(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 24 * 60 * 60 {
        let days = secs / (24 * 60 * 60);
        let hours = (secs % (24 * 60 * 60)) / 3600;
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d{hours}h")
        }
    } else if secs >= 3600 {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes}m")
        }
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn benchmark_job_kind_label(kind: &BenchmarkJobKind) -> &'static str {
    match kind {
        BenchmarkJobKind::Group => "group",
        BenchmarkJobKind::AutoSelect => "auto",
        BenchmarkJobKind::SingleNode { .. } => "single",
    }
}

fn centered_rect(width: u16, height: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);
    horizontal
}

fn truncate_for_width(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let width = unicode_width::UnicodeWidthStr::width(value);
    if width <= max_width {
        return value.to_string();
    }
    let mut output = String::new();
    let mut current_width = 0;
    for ch in value.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + char_width + 1 > max_width {
            break;
        }
        output.push(ch);
        current_width += char_width;
    }
    output.push('…');
    output
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

fn default_system_proxy_server(config_path: &Path) -> String {
    env::var("SING_BOX_TUI_SYSTEM_PROXY_SERVER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            detect_mixed_inbound_proxy_server(config_path)
                .unwrap_or_else(|| "127.0.0.1:6780".to_string())
        })
}

fn detect_mixed_inbound_proxy_server(config_path: &Path) -> Option<String> {
    let text = fs::read_to_string(config_path).ok()?;
    detect_mixed_inbound_proxy_server_from_json(&text)
        .or_else(|| detect_mixed_inbound_proxy_server_from_text(&text))
}

fn detect_mixed_inbound_proxy_server_from_json(text: &str) -> Option<String> {
    let config: Value = serde_json::from_str(text).ok()?;
    let inbounds = config.get("inbounds")?.as_array()?;
    let inbound = inbounds
        .iter()
        .find(|inbound| inbound.get("type").and_then(Value::as_str) == Some("mixed"))?;
    let port = inbound.get("listen_port")?.as_u64()?;
    let listen = inbound
        .get("listen")
        .and_then(Value::as_str)
        .unwrap_or("127.0.0.1");
    let host = match listen {
        "::" | "0.0.0.0" | "" => "127.0.0.1",
        value => value,
    };
    Some(format!("{host}:{port}"))
}

fn detect_mixed_inbound_proxy_server_from_text(text: &str) -> Option<String> {
    let inbounds_key = text.find("\"inbounds\"")?;
    let after_key = &text[inbounds_key..];
    let array_start = after_key.find('[')? + inbounds_key;
    let array_end = find_json_array_end(text, array_start)?;
    let inbounds = &text[array_start..=array_end];
    let mixed_index = inbounds.find("\"mixed\"")?;
    let before_mixed = &inbounds[..mixed_index];
    let object_start = before_mixed.rfind('{')?;
    let after_object_start = &inbounds[object_start..];
    let object_end = find_json_object_end(after_object_start, 0)?;
    let inbound = &after_object_start[..=object_end];
    let port = find_json_u16_field(inbound, "listen_port")?;
    let listen = find_json_string_field(inbound, "listen").unwrap_or("127.0.0.1");
    let host = match listen {
        "::" | "0.0.0.0" | "" => "127.0.0.1",
        value => value,
    };
    Some(format!("{host}:{port}"))
}

fn find_json_array_end(text: &str, start: usize) -> Option<usize> {
    find_json_container_end(text, start, '[', ']')
}

fn find_json_object_end(text: &str, start: usize) -> Option<usize> {
    find_json_container_end(text, start, '{', '}')
}

fn find_json_container_end(text: &str, start: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
        } else if ch == open {
            depth += 1;
        } else if ch == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(start + offset);
            }
        }
    }
    None
}

fn find_json_u16_field<'a>(object: &'a str, field: &str) -> Option<&'a str> {
    let key = format!("\"{field}\"");
    let key_index = object.find(&key)?;
    let after_key = &object[key_index + key.len()..];
    let colon_index = after_key.find(':')?;
    let value = after_key[colon_index + 1..].trim_start();
    let end = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    let port = &value[..end];
    if port.is_empty() { None } else { Some(port) }
}

fn find_json_string_field<'a>(object: &'a str, field: &str) -> Option<&'a str> {
    let key = format!("\"{field}\"");
    let key_index = object.find(&key)?;
    let after_key = &object[key_index + key.len()..];
    let colon_index = after_key.find(':')?;
    let value = after_key[colon_index + 1..].trim_start();
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(&value[..end])
}

fn system_proxy_bypass_entries(entries: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut bypass = Vec::new();
    for entry in DEFAULT_SYSTEM_PROXY_BYPASS {
        push_unique_system_proxy_bypass_value(&mut bypass, &mut seen, entry);
    }
    push_cgnat_system_proxy_bypass_values(&mut bypass, &mut seen);
    for entry in entries {
        push_system_proxy_bypass_entry(&mut bypass, &mut seen, entry);
    }
    bypass
}

fn push_cgnat_system_proxy_bypass_values(bypass: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    for octet in 64..=127 {
        push_unique_system_proxy_bypass_value(bypass, seen, &format!("100.{octet}.*"));
    }
}

fn push_system_proxy_bypass_entry(
    bypass: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    entry: &str,
) {
    let entry = entry.trim();
    if entry.is_empty() {
        return;
    }
    push_unique_system_proxy_bypass_value(bypass, seen, entry);
    if !entry.contains('*')
        && !entry.contains('/')
        && !entry.starts_with('<')
        && !entry.parse::<std::net::IpAddr>().is_ok()
    {
        push_unique_system_proxy_bypass_value(bypass, seen, &format!("*.{entry}"));
    }
}

fn push_unique_system_proxy_bypass_value(
    bypass: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
    value: &str,
) {
    let value = value.trim().to_ascii_lowercase();
    if !value.is_empty() && seen.insert(value.clone()) {
        bypass.push(value);
    }
}

#[cfg(windows)]
fn wininet_proxy_override_entries(entries: &[String]) -> Vec<String> {
    let mut entries = system_proxy_bypass_entries(entries);
    if !entries.iter().any(|entry| entry == "<local>") {
        entries.push("<local>".to_string());
    }
    entries
}

#[cfg(windows)]
fn run_system_proxy_update(
    server: &str,
    enable: bool,
    bypass_entries: &[String],
) -> Result<String> {
    let script = windows_system_proxy_script_path()
        .with_context(|| "failed to locate scripts/windows/set-system-proxy.ps1")?;
    let action = if enable { "-Enable" } else { "-Disable" };
    let mut args = vec![
        "-NoProfile".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-File".to_string(),
        script
            .to_str()
            .context("system proxy script path is not valid UTF-8")?
            .to_string(),
        action.to_string(),
    ];
    if enable {
        args.extend(["-Server".to_string(), server.to_string()]);
        let override_list = wininet_proxy_override_entries(bypass_entries).join(";");
        args.extend(["-Override".to_string(), override_list]);
    }
    let output = Command::new("powershell.exe")
        .args(args)
        .output()
        .context("failed to run PowerShell system proxy script")?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        return Ok(if stdout.is_empty() {
            format!("Enabled Windows system proxy: {server}")
        } else {
            stdout
        });
    }

    let message = if stderr.is_empty() { stdout } else { stderr };
    bail!(
        "PowerShell exited with {}: {}",
        output.status.code().unwrap_or(-1),
        message
    )
}

#[cfg(target_os = "macos")]
fn run_system_proxy_update(
    server: &str,
    enable: bool,
    bypass_entries: &[String],
) -> Result<String> {
    let services = macos_system_proxy_services()?;
    if services.is_empty() {
        bail!("no enabled macOS network services found");
    }

    if enable {
        let (host, port) = parse_proxy_server(server)?;
        let bypass = system_proxy_bypass_entries(bypass_entries);
        for service in &services {
            run_networksetup(&["-setwebproxy", service, &host, &port])?;
            run_networksetup(&["-setsecurewebproxy", service, &host, &port])?;
            run_networksetup(&["-setsocksfirewallproxy", service, &host, &port])?;
            let mut args = vec!["-setproxybypassdomains", service.as_str()];
            args.extend(bypass.iter().map(String::as_str));
            run_networksetup(&args)?;
        }
        Ok(format!(
            "Enabled macOS system proxy for {} at {server}",
            services.join(", ")
        ))
    } else {
        for service in &services {
            run_networksetup(&["-setwebproxystate", service, "off"])?;
            run_networksetup(&["-setsecurewebproxystate", service, "off"])?;
            run_networksetup(&["-setsocksfirewallproxystate", service, "off"])?;
        }
        Ok(format!(
            "Disabled macOS system proxy for {}",
            services.join(", ")
        ))
    }
}

#[cfg(target_os = "linux")]
fn run_system_proxy_update(
    server: &str,
    enable: bool,
    bypass_entries: &[String],
) -> Result<String> {
    if !command_exists("gsettings") {
        bail!("Linux system proxy toggle requires gsettings");
    }

    if enable {
        let (host, port) = parse_proxy_server(server)?;
        let host_value = gsettings_string_value(&host);
        run_gsettings(&["set", "org.gnome.system.proxy", "mode", "manual"])?;
        run_gsettings(&["set", "org.gnome.system.proxy.http", "host", &host_value])?;
        run_gsettings(&["set", "org.gnome.system.proxy.http", "port", &port])?;
        run_gsettings(&["set", "org.gnome.system.proxy.https", "host", &host_value])?;
        run_gsettings(&["set", "org.gnome.system.proxy.https", "port", &port])?;
        run_gsettings(&["set", "org.gnome.system.proxy.socks", "host", &host_value])?;
        run_gsettings(&["set", "org.gnome.system.proxy.socks", "port", &port])?;
        let ignore_hosts =
            gsettings_string_list_value(&system_proxy_bypass_entries(bypass_entries));
        run_gsettings(&[
            "set",
            "org.gnome.system.proxy",
            "ignore-hosts",
            &ignore_hosts,
        ])?;
        Ok(format!(
            "Enabled Linux system proxy via gsettings: {server}"
        ))
    } else {
        run_gsettings(&["set", "org.gnome.system.proxy", "mode", "none"])?;
        Ok("Disabled Linux system proxy via gsettings".to_string())
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn run_system_proxy_update(
    _server: &str,
    _enable: bool,
    _bypass_entries: &[String],
) -> Result<String> {
    bail!("system proxy toggle is only available on Windows, macOS, and Linux")
}

struct SingBoxRestartResult {
    restarted_pids: Vec<u32>,
    started_pid: u32,
    child: Child,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SingBoxRestartSummary {
    restarted_pids: Vec<u32>,
    started_pid: u32,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn restart_sing_box_for_config(config_path: &Path) -> Result<SingBoxRestartResult> {
    let pids = find_sing_box_run_pids_for_config(config_path)?;
    for pid in &pids {
        stop_sing_box_pid(*pid)
            .with_context(|| format!("failed to stop existing sing-box process {pid}"))?;
    }

    let log_path = sing_box_process_log_path(config_path);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open sing-box process log {}", log_path.display()))?;
    let stderr = log.try_clone().with_context(|| {
        format!(
            "failed to clone sing-box process log {}",
            log_path.display()
        )
    })?;
    let mut child = Command::new("sing-box")
        .arg("run")
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| {
            format!(
                "failed to start sing-box run --config {}",
                config_path.display()
            )
        })?;
    let started_pid = child.id();
    std::thread::sleep(Duration::from_millis(500));
    if let Some(status) = child
        .try_wait()
        .context("failed to inspect restarted sing-box process")?
    {
        bail!(
            "sing-box exited immediately with {status}; see {}",
            log_path.display()
        );
    }
    Ok(SingBoxRestartResult {
        restarted_pids: pids,
        started_pid,
        child,
    })
}

#[cfg(windows)]
fn restart_sing_box_for_config(config_path: &Path) -> Result<SingBoxRestartResult> {
    let pids = find_sing_box_run_pids_for_config(config_path)?;
    for pid in &pids {
        stop_sing_box_pid(*pid)
            .with_context(|| format!("failed to stop existing sing-box process {pid}"))?;
    }

    let log_path = sing_box_process_log_path(config_path);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open sing-box process log {}", log_path.display()))?;
    let stderr = log.try_clone().with_context(|| {
        format!(
            "failed to clone sing-box process log {}",
            log_path.display()
        )
    })?;
    let mut child = Command::new("sing-box")
        .arg("run")
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| {
            format!(
                "failed to start sing-box run --config {}",
                config_path.display()
            )
        })?;
    let started_pid = child.id();
    std::thread::sleep(Duration::from_millis(500));
    if let Some(status) = child
        .try_wait()
        .context("failed to inspect restarted sing-box process")?
    {
        bail!(
            "sing-box exited immediately with {status}; see {}",
            log_path.display()
        );
    }
    Ok(SingBoxRestartResult {
        restarted_pids: pids,
        started_pid,
        child,
    })
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn restart_sing_box_for_config(_config_path: &Path) -> Result<SingBoxRestartResult> {
    bail!("automatic sing-box restart is only available on Windows, macOS, and Linux")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn find_sing_box_run_pids_for_config(config_path: &Path) -> Result<Vec<u32>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .context("failed to list processes with ps")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pids = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((pid, command)) = parse_ps_pid_command(line) else {
            continue;
        };
        if command_matches_sing_box_run_for_config(command, config_path) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

#[cfg(windows)]
fn find_sing_box_run_pids_for_config(config_path: &Path) -> Result<Vec<u32>> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name = 'sing-box.exe'\" | Select-Object ProcessId,CommandLine | ConvertTo-Json -Compress",
        ])
        .output()
        .context("failed to list sing-box processes with PowerShell")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "PowerShell process query exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }
    parse_windows_process_json(&String::from_utf8_lossy(&output.stdout), config_path)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_ps_pid_command(line: &str) -> Option<(u32, &str)> {
    let mut parts = line.trim().splitn(2, char::is_whitespace);
    let pid = parts.next()?.parse::<u32>().ok()?;
    let command = parts.next()?.trim();
    Some((pid, command))
}

#[cfg(windows)]
fn parse_windows_process_json(text: &str, config_path: &Path) -> Result<Vec<u32>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value =
        serde_json::from_str(text).context("failed to parse PowerShell process JSON")?;
    let processes = match &value {
        Value::Array(processes) => processes.iter().collect::<Vec<_>>(),
        Value::Object(_) => vec![&value],
        _ => bail!("PowerShell process JSON had unexpected shape"),
    };
    let mut pids = Vec::new();
    for process in processes {
        let command = process
            .get("CommandLine")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if command_matches_sing_box_run_for_config(command, config_path)
            && let Some(pid) = process.get("ProcessId").and_then(Value::as_u64)
        {
            pids.push(pid as u32);
        }
    }
    Ok(pids)
}

fn command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for character in command.chars() {
        match character {
            '"' => in_quotes = !in_quotes,
            value if value.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            value => current.push(value),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn command_program_name_matches(program: &str, expected: &str) -> bool {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.eq_ignore_ascii_case(expected)
                || name
                    .strip_suffix(".exe")
                    .is_some_and(|base| base.eq_ignore_ascii_case(expected))
        })
        .unwrap_or(false)
}

fn command_matches_sing_box_run_for_config(command: &str, config_path: &Path) -> bool {
    let args = command_tokens(command);
    if args.len() < 3 {
        return false;
    }
    let program_is_sing_box = args
        .first()
        .is_some_and(|program| command_program_name_matches(program, "sing-box"));
    if !program_is_sing_box || !args.iter().any(|arg| arg == "run") {
        return false;
    }
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let config_values = sing_box_config_args(&arg_refs);
    !config_values.is_empty()
        && config_values
            .iter()
            .any(|value| config_arg_matches_path(value, config_path))
}

fn sing_box_config_args<'a>(args: &'a [&'a str]) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--config" | "-c" => {
                if let Some(value) = args.get(i + 1) {
                    values.push(*value);
                    i += 1;
                }
            }
            value if value.starts_with("--config=") => {
                values.push(value.trim_start_matches("--config="));
            }
            value if value.starts_with("-c=") => {
                values.push(value.trim_start_matches("-c="));
            }
            _ => {}
        }
        i += 1;
    }
    values
}

fn config_arg_matches_path(value: &str, config_path: &Path) -> bool {
    let value_path = Path::new(value);
    if value_path == config_path {
        return true;
    }
    if let Ok(canonical_config) = config_path.canonicalize()
        && let Ok(canonical_value) = value_path.canonicalize()
        && canonical_value == canonical_config
    {
        return true;
    }
    config_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|file_name| {
            value == file_name
                || value == format!("./{file_name}")
                || value.rsplit(['/', '\\']).next() == Some(file_name)
        })
}

#[cfg_attr(windows, allow(dead_code))]
fn command_matches_headless_auto_pick(command: &str) -> bool {
    let args = command_tokens(command);
    if args.len() < 3 {
        return false;
    }
    let program_is_sing_box_tui = args
        .first()
        .is_some_and(|program| command_program_name_matches(program, "sing-box-tui"));
    program_is_sing_box_tui
        && args.iter().any(|arg| arg == "run")
        && args.iter().any(|arg| arg == "--headless-auto-pick")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn background_process_command(pid: u32) -> Result<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .with_context(|| format!("failed to inspect background process {pid}"))?;
    if !output.status.success() {
        bail!(
            "failed to inspect background process {pid}: ps exited with {}",
            output.status
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ensure_background_pid_matches_worker(pid: u32) -> Result<()> {
    let command = background_process_command(pid)?;
    if command_matches_headless_auto_pick(&command) {
        return Ok(());
    }
    bail!("background pid {pid} is not a sing-box-tui headless auto-pick worker")
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn wait_for_processes_to_exit(pids: &[u32]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if pids.iter().all(|pid| !process_exists(*pid)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for sing-box process(es) to exit: {pids:?}")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn wait_for_background_process_to_exit(pid: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out waiting for background process {pid} to exit")
}

#[cfg(windows)]
fn wait_for_background_process_to_exit(pid: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out waiting for background process {pid} to exit")
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn wait_for_background_process_to_exit(_pid: u32, _timeout: Duration) -> Result<()> {
    bail!("background worker shutdown is only available on macOS and Linux")
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

#[cfg(all(windows, not(test)))]
const OFFICIAL_SONICWALL_CLIENT_PROCESSES: &[&str] = &["SnwlVpn.exe", "SnwlConnect.exe"];

#[cfg(all(windows, not(test)))]
fn running_official_sonicwall_client_processes() -> Vec<String> {
    OFFICIAL_SONICWALL_CLIENT_PROCESSES
        .iter()
        .filter_map(|process_name| {
            let output = Command::new("tasklist")
                .args([
                    "/FI",
                    &format!("IMAGENAME eq {process_name}"),
                    "/FO",
                    "CSV",
                    "/NH",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            parse_windows_tasklist_image_names(&String::from_utf8_lossy(&output.stdout))
                .into_iter()
                .any(|name| name.eq_ignore_ascii_case(process_name))
                .then(|| (*process_name).to_string())
        })
        .collect()
}

#[cfg(any(not(windows), test))]
fn running_official_sonicwall_client_processes() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
fn parse_windows_tasklist_image_names(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix('"')?;
            let (name, _) = line.split_once("\",")?;
            Some(name.to_string())
        })
        .collect()
}

#[cfg(not(windows))]
fn parse_windows_tasklist_image_names(_text: &str) -> Vec<String> {
    Vec::new()
}

fn format_official_sonicwall_client_warning(processes: &[String]) -> String {
    format!(
        "检测到官方 SonicWall 客户端仍在运行: {}。请先退出官方客户端，再启动 TUI 的 SonicWall 连接。",
        processes.join(", ")
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_background_pid(pid: u32) -> Result<()> {
    ensure_background_pid_matches_worker(pid)?;
    let status = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to stop background process {pid}"))?;
    if !status.success() {
        bail!("failed to stop background process {pid}: kill exited with {status}");
    }
    if wait_for_processes_to_exit(&[pid]).is_ok() {
        return Ok(());
    }
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to force stop background process {pid}"))?;
    if !status.success() {
        bail!("failed to force stop background process {pid}: kill -9 exited with {status}");
    }
    wait_for_processes_to_exit(&[pid])
}

#[cfg(windows)]
fn stop_background_pid(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .with_context(|| format!("failed to force stop background process {pid}"))?;
    if !status.success() && process_exists(pid) {
        bail!("failed to force stop background process {pid}: taskkill exited with {status}");
    }
    wait_for_background_process_to_exit(pid, Duration::from_secs(3))
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn stop_background_pid(_pid: u32) -> Result<()> {
    bail!("background worker shutdown is only available on macOS and Linux")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_sing_box_pid(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to stop sing-box process {pid}"))?;
    if !status.success() {
        bail!("failed to stop sing-box process {pid}: kill exited with {status}");
    }
    if wait_for_processes_to_exit(&[pid]).is_ok() {
        return Ok(());
    }
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to force stop sing-box process {pid}"))?;
    if !status.success() {
        bail!("failed to force stop sing-box process {pid}: kill -9 exited with {status}");
    }
    wait_for_processes_to_exit(&[pid])
}

#[cfg(windows)]
fn stop_sing_box_pid(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .status()
        .with_context(|| format!("failed to stop sing-box process {pid}"))?;
    if status.success() && wait_for_processes_to_exit(&[pid]).is_ok() {
        return Ok(());
    }
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .with_context(|| format!("failed to force stop sing-box process {pid}"))?;
    if !status.success() {
        bail!("failed to force stop sing-box process {pid}: taskkill exited with {status}");
    }
    wait_for_processes_to_exit(&[pid])
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn stop_sing_box_pid(_pid: u32) -> Result<()> {
    bail!("managed sing-box shutdown is only available on macOS and Linux")
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn stop_sing_box_child(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("failed to inspect managed sing-box process")?
        .is_some()
    {
        return Ok(());
    }
    child
        .kill()
        .context("failed to kill managed sing-box process")?;
    child
        .wait()
        .context("failed to reap managed sing-box process")?;
    Ok(())
}

fn sing_box_process_log_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("sing-box.log")
}

#[cfg(windows)]
fn system_proxy_matches(server: &str) -> bool {
    let output = Command::new("reg.exe")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    registry_value_line(&text, "ProxyEnable")
        .is_some_and(|line| line.split_whitespace().last() == Some("0x1"))
        && registry_value_line(&text, "ProxyServer").is_some_and(|line| {
            line.split_once("REG_SZ")
                .map(|(_, value)| value.trim() == server)
                .unwrap_or(false)
        })
}

#[cfg(target_os = "macos")]
fn system_proxy_matches(server: &str) -> bool {
    let Ok((host, port)) = parse_proxy_server(server) else {
        return false;
    };
    let output = Command::new("scutil").arg("--proxy").output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    macos_proxy_value(&text, "HTTPEnable") == Some("1")
        && macos_proxy_value(&text, "HTTPProxy") == Some(host.as_str())
        && macos_proxy_value(&text, "HTTPPort") == Some(port.as_str())
        && macos_proxy_value(&text, "HTTPSEnable") == Some("1")
        && macos_proxy_value(&text, "HTTPSProxy") == Some(host.as_str())
        && macos_proxy_value(&text, "HTTPSPort") == Some(port.as_str())
        && macos_proxy_value(&text, "SOCKSEnable") == Some("1")
        && macos_proxy_value(&text, "SOCKSProxy") == Some(host.as_str())
        && macos_proxy_value(&text, "SOCKSPort") == Some(port.as_str())
}

#[cfg(target_os = "linux")]
fn system_proxy_matches(server: &str) -> bool {
    let Ok((host, port)) = parse_proxy_server(server) else {
        return false;
    };
    gsettings_value("org.gnome.system.proxy", "mode").as_deref() == Some("manual")
        && gsettings_value("org.gnome.system.proxy.http", "host").as_deref() == Some(host.as_str())
        && gsettings_value("org.gnome.system.proxy.http", "port").as_deref() == Some(port.as_str())
        && gsettings_value("org.gnome.system.proxy.https", "host").as_deref() == Some(host.as_str())
        && gsettings_value("org.gnome.system.proxy.https", "port").as_deref() == Some(port.as_str())
        && gsettings_value("org.gnome.system.proxy.socks", "host").as_deref() == Some(host.as_str())
        && gsettings_value("org.gnome.system.proxy.socks", "port").as_deref() == Some(port.as_str())
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn system_proxy_matches(_server: &str) -> bool {
    false
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_proxy_server(server: &str) -> Result<(String, String)> {
    let server = server.trim();
    let (host, port) = server
        .rsplit_once(':')
        .with_context(|| format!("system proxy server must be host:port, got {server}"))?;
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = port.trim().to_string();
    if host.is_empty() {
        bail!("system proxy server host is empty");
    }
    let parsed_port = port
        .parse::<u16>()
        .with_context(|| format!("system proxy server port must be a number, got {port}"))?;
    if parsed_port == 0 {
        bail!("system proxy server port must be greater than 0");
    }
    Ok((host, port))
}

#[cfg(target_os = "linux")]
fn run_gsettings(args: &[&str]) -> Result<()> {
    let output = Command::new("gsettings")
        .args(args)
        .output()
        .with_context(|| format!("failed to run gsettings {}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    bail!(
        "gsettings {} exited with {}: {}",
        args.join(" "),
        output.status.code().unwrap_or(-1),
        message
    )
}

#[cfg(target_os = "linux")]
fn gsettings_value(schema: &str, key: &str) -> Option<String> {
    let output = Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(parse_gsettings_scalar(&value).to_string())
}

#[cfg(target_os = "linux")]
fn parse_gsettings_scalar(value: &str) -> &str {
    value.trim().trim_start_matches('\'').trim_end_matches('\'')
}

#[cfg(target_os = "linux")]
fn gsettings_string_value(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(target_os = "linux")]
fn gsettings_string_list_value(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| gsettings_string_value(value))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{values}]")
}

#[cfg(target_os = "linux")]
fn command_exists(command: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|path| path.join(command).is_file()))
}

#[cfg(target_os = "macos")]
fn macos_system_proxy_services() -> Result<Vec<String>> {
    if let Ok(value) = env::var("SING_BOX_TUI_SYSTEM_PROXY_SERVICE") {
        let services = value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if !services.is_empty() {
            return Ok(services);
        }
    }

    let output = Command::new("networksetup")
        .arg("-listallnetworkservices")
        .output()
        .context("failed to list macOS network services")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let message = if stderr.is_empty() { stdout } else { stderr };
        bail!(
            "networksetup -listallnetworkservices exited with {}: {}",
            output.status.code().unwrap_or(-1),
            message
        );
    }

    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("An asterisk"))
        .filter(|line| !line.starts_with('*'))
        .map(ToString::to_string)
        .collect())
}

#[cfg(target_os = "macos")]
fn run_networksetup(args: &[&str]) -> Result<()> {
    let output = Command::new("networksetup")
        .args(args)
        .output()
        .with_context(|| format!("failed to run networksetup {}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    bail!(
        "networksetup {} exited with {}: {}",
        args.join(" "),
        output.status.code().unwrap_or(-1),
        message
    )
}

#[cfg(target_os = "macos")]
fn macos_proxy_value<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim() == name).then_some(value.trim())
    })
}

#[cfg(windows)]
fn registry_value_line<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|value| value.eq_ignore_ascii_case(name))
    })
}

#[cfg(windows)]
fn windows_system_proxy_script_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("SING_BOX_TUI_SYSTEM_PROXY_SCRIPT") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let cwd_path = PathBuf::from("scripts")
        .join("windows")
        .join("set-system-proxy.ps1");
    if cwd_path.exists() {
        return Some(cwd_path);
    }

    let exe = env::current_exe().ok()?;
    for ancestor in exe.ancestors() {
        let candidate = ancestor
            .join("scripts")
            .join("windows")
            .join("set-system-proxy.ps1");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Focus {
    Groups,
    Members,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeftPaneSection {
    Internet,
    Intranet,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum IntranetDetailSection {
    Dns,
    Routes,
    Domains,
}

impl IntranetDetailSection {
    fn key(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Routes => "routes",
            Self::Domains => "domains",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IntranetDetailSectionRange {
    section: IntranetDetailSection,
    start: usize,
    end: usize,
    foldable: bool,
}

struct IntranetDetailView {
    lines: Vec<Line<'static>>,
    sections: Vec<IntranetDetailSectionRange>,
}

#[derive(Clone, Debug)]
struct LatencyChartState {
    selector: String,
    node: String,
    samples: Vec<NodeLatencySample>,
    window: Duration,
    last_refresh: Instant,
}

struct OnboardingState {
    input: String,
    message: String,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SettingsField {
    BenchmarkUrl,
    BenchmarkTimeoutMs,
    RequestTimeoutSec,
    MaxConcurrency,
    VerifyTargets,
    AutoPickThresholdMs,
    AutoPickIntervalSec,
    SystemProxyServer,
    PrivateAccessProfile,
    PrivateAccessManifestPath,
    PrivateAccessMode,
    PrivateAccessServer,
    PrivateAccessPort,
    PrivateAccessUsername,
    PrivateAccessPassword,
    PrivateAccessPasswordEnv,
    PrivateAccessBridgeListen,
    PrivateAccessTlsVerify,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrivateAccessMode {
    Bridge,
    Tun,
}

impl PrivateAccessMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Tun => "tun",
        }
    }
}

fn parse_private_access_mode(value: &str) -> Result<PrivateAccessMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "bridge" | "http_bridge" | "http-bridge" => Ok(PrivateAccessMode::Bridge),
        "tun" => Ok(PrivateAccessMode::Tun),
        _ => bail!("private access mode must be bridge or tun"),
    }
}

fn helper_command_uses_interactive_sudo(command: &[String]) -> bool {
    command.first().is_some_and(|program| program == "sudo")
        && !command.iter().skip(1).any(|arg| arg == "-n")
}

fn default_tui_tun_helper_command() -> Vec<String> {
    let exe = env::current_exe()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| "sing-box-tui".to_string());
    let helper_args = vec![
        exe,
        "private-access-tun-helper".to_string(),
        "--stdio".to_string(),
    ];
    if tun_helper_needs_sudo() {
        let mut command = vec!["sudo".to_string()];
        command.extend(helper_args);
        command
    } else {
        helper_args
    }
}

#[cfg(unix)]
fn tun_helper_needs_sudo() -> bool {
    unsafe { libc::geteuid() != 0 }
}

#[cfg(not(unix))]
fn tun_helper_needs_sudo() -> bool {
    false
}

struct SettingsEditState {
    field: SettingsField,
    input: String,
    error: Option<String>,
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
    last_background_status_refresh: Instant,
    last_background_status_generation: u64,
    background_started_at_unix: u64,
    background_worker: Option<BackgroundWorkerRuntime>,
    background_status_job: Option<BackgroundStatusPollJob>,
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
    system_proxy_server: String,
    system_proxy_server_override: bool,
    system_proxy_enabled: bool,
    system_proxy_job: Option<SystemProxyJob>,
    last_system_proxy_status_refresh: Instant,
    verify_job: Option<VerifyJob>,
    sing_box: SingBoxProcessRuntime,
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

struct SystemProxyJob {
    server: String,
    enable: bool,
    receiver: mpsc::Receiver<Result<String, String>>,
    worker: JoinHandle<()>,
}

struct VerifyJob {
    receiver: mpsc::Receiver<VerificationReport>,
    worker: JoinHandle<()>,
}

struct SingBoxProcessRuntime {
    managed_pid: Option<u32>,
    managed_child: Option<Child>,
    keep_running: bool,
}

impl SingBoxProcessRuntime {
    fn new(keep_running: bool) -> Self {
        Self {
            managed_pid: None,
            managed_child: None,
            keep_running,
        }
    }
}

#[derive(Clone, Debug)]
struct PrivateAccessProgressModal {
    profile_index: usize,
    title: String,
    entries: Vec<PrivateAccessProgressEntry>,
    done: bool,
}

struct PrivateAccessAuthModal {
    profile_index: usize,
    service: String,
    session_id: String,
    challenge_id: String,
    title: String,
    message: String,
    fields: Vec<PrivateAccessAuthField>,
    buttons: Vec<String>,
    inputs: Vec<String>,
    field_index: usize,
    error: Option<String>,
}

impl Drop for PrivateAccessAuthModal {
    fn drop(&mut self) {
        self.session_id.zeroize();
        self.challenge_id.zeroize();
        self.inputs.zeroize();
    }
}

#[derive(Clone, Debug)]
struct PrivateAccessProgressEntry {
    tone: PrivateAccessProgressTone,
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateAccessProgressTone {
    Info,
    Success,
    Error,
}

impl PrivateAccessProgressTone {
    fn prefix(self) -> &'static str {
        match self {
            Self::Info => "[..] ",
            Self::Success => "[OK] ",
            Self::Error => "[ERR] ",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Info => Style::default().fg(Color::Cyan),
            Self::Success => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Self::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        }
    }
}

struct PrivateAccessProfileRuntime {
    id: String,
    manifest_path: Option<String>,
    mode: PrivateAccessMode,
    manifest: PrivateAccessServiceManifest,
    process: Option<ExternalPrivateAccessService>,
    state: PrivateAccessState,
    server: String,
    port: u16,
    username: String,
    password: String,
    password_env: String,
    bridge_listen: String,
    tun_helper: Vec<String>,
    tls_verify: bool,
    routes: Vec<PrivateAccessRoute>,
    dns: Vec<String>,
    domains: Vec<String>,
    domain_suffixes: Vec<String>,
    bridge: Option<PrivateAccessBridge>,
    last_error: Option<String>,
    integration_failed: bool,
    background_pid: Option<u32>,
}

impl PrivateAccessProfileRuntime {
    #[cfg(test)]
    fn default_hillstone() -> Result<Self> {
        let manifest_path = env::var("SING_BOX_TUI_PRIVATE_ACCESS_MANIFEST")
            .ok()
            .filter(|path| !path.trim().is_empty());
        let manifest =
            load_private_access_manifest_for_profile("hillstone", manifest_path.as_deref())?;
        Ok(Self {
            id: "hillstone".to_string(),
            manifest_path,
            mode: PrivateAccessMode::Bridge,
            manifest,
            process: None,
            state: PrivateAccessState::Disconnected,
            server: env::var("HILLSTONE_SERVER").unwrap_or_default(),
            port: 4433,
            username: env::var("HILLSTONE_USERNAME").unwrap_or_default(),
            password: String::new(),
            password_env: "HILLSTONE_PASSWORD".to_string(),
            bridge_listen: "127.0.0.1:16780".to_string(),
            tun_helper: Vec::new(),
            tls_verify: false,
            routes: Vec::new(),
            dns: Vec::new(),
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            bridge: None,
            last_error: None,
            integration_failed: false,
            background_pid: None,
        })
    }

    #[cfg(test)]
    fn default_sonicwall() -> Result<Self> {
        let manifest_path = env::var("SING_BOX_TUI_SONICWALL_MANIFEST")
            .ok()
            .filter(|path| !path.trim().is_empty());
        let manifest =
            load_private_access_manifest_for_profile("sonicwall", manifest_path.as_deref())?;
        Ok(Self {
            id: "sonicwall".to_string(),
            manifest_path,
            mode: PrivateAccessMode::Tun,
            manifest,
            process: None,
            state: PrivateAccessState::Disconnected,
            server: "sslvpn.hundsun.com".to_string(),
            port: 443,
            username: String::new(),
            password: String::new(),
            password_env: String::new(),
            bridge_listen: String::new(),
            tun_helper: Vec::new(),
            tls_verify: true,
            routes: Vec::new(),
            dns: Vec::new(),
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            bridge: None,
            last_error: None,
            integration_failed: false,
            background_pid: None,
        })
    }

    fn from_state(state: PrivateAccessProfileState) -> Result<Self> {
        let id = normalize_optional_setting(Some(state.id.clone()))
            .unwrap_or_else(|| "hillstone".to_string());
        let manifest_path = normalize_optional_setting(state.manifest_path.clone());
        let manifest = load_private_access_manifest_for_profile(&id, manifest_path.as_deref())?;
        let is_sonicwall = manifest.id == "sonicwall";
        let mut profile = Self {
            id,
            manifest_path,
            mode: if is_sonicwall {
                PrivateAccessMode::Tun
            } else {
                PrivateAccessMode::Bridge
            },
            manifest,
            process: None,
            state: PrivateAccessState::Disconnected,
            server: if is_sonicwall {
                "sslvpn.hundsun.com".to_string()
            } else {
                String::new()
            },
            port: if is_sonicwall { 443 } else { 4433 },
            username: String::new(),
            password: String::new(),
            password_env: String::new(),
            bridge_listen: if is_sonicwall {
                String::new()
            } else {
                "127.0.0.1:16780".to_string()
            },
            tun_helper: Vec::new(),
            tls_verify: is_sonicwall,
            routes: Vec::new(),
            dns: Vec::new(),
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            bridge: None,
            last_error: None,
            integration_failed: false,
            background_pid: None,
        };
        profile.apply_state(state)?;
        if profile.manifest.id == "sonicwall" {
            profile.mode = PrivateAccessMode::Tun;
        }
        Ok(profile)
    }

    fn apply_state(&mut self, state: PrivateAccessProfileState) -> Result<()> {
        if let Some(value) = normalize_optional_setting(state.mode) {
            self.mode = parse_private_access_mode(&value)?;
        }
        if let Some(value) = normalize_optional_setting(state.server) {
            self.server = value;
        }
        if let Some(value) = state.port.filter(|value| *value > 0) {
            self.port = value;
        }
        if let Some(value) = normalize_optional_setting(state.username) {
            self.username = value;
        }
        if let Some(value) = normalize_optional_setting(state.password) {
            self.password = value;
        }
        if let Some(value) = normalize_optional_setting(state.password_env) {
            self.password_env = value;
        }
        if let Some(value) = normalize_optional_setting(state.bridge_listen) {
            self.bridge_listen = value;
        }
        if let Some(values) = state.tun_helper {
            self.tun_helper = values
                .into_iter()
                .filter(|value| !value.trim().is_empty())
                .collect();
        }
        self.tls_verify = state.tls_verify;
        self.background_pid = state.background_pid.filter(|pid| process_exists(*pid));
        if self.background_pid.is_some() {
            self.state = PrivateAccessState::Connected;
        }
        Ok(())
    }

    fn runtime_state(&self) -> PrivateAccessProfileState {
        PrivateAccessProfileState {
            id: self.id.clone(),
            manifest_path: self.manifest_path.clone(),
            mode: Some(self.mode.as_str().to_string()),
            server: normalize_optional_setting(Some(self.server.clone())),
            port: Some(self.port),
            username: normalize_optional_setting(Some(self.username.clone())),
            password: normalize_optional_setting(Some(self.password.clone())),
            password_env: normalize_optional_setting(Some(self.password_env.clone())),
            bridge_listen: normalize_optional_setting(Some(self.bridge_listen.clone())),
            tun_helper: if self.tun_helper.is_empty() {
                None
            } else {
                Some(self.tun_helper.clone())
            },
            tls_verify: self.tls_verify,
            background_pid: self.background_pid.filter(|pid| process_exists(*pid)),
        }
    }
}

struct PrivateAccessRuntime {
    profiles: Vec<PrivateAccessProfileRuntime>,
    focused_index: usize,
}

impl PrivateAccessRuntime {
    fn new() -> Result<Self> {
        Ok(Self {
            profiles: Vec::new(),
            focused_index: 0,
        })
    }

    #[cfg(test)]
    fn with_default_hillstone() -> Result<Self> {
        Ok(Self {
            profiles: vec![PrivateAccessProfileRuntime::default_hillstone()?],
            focused_index: 0,
        })
    }

    fn is_configured(&self) -> bool {
        !self.profiles.is_empty()
    }

    fn focused(&self) -> &PrivateAccessProfileRuntime {
        &self.profiles[self.focused_index]
    }

    fn focused_mut(&mut self) -> &mut PrivateAccessProfileRuntime {
        &mut self.profiles[self.focused_index]
    }

    fn focused_opt(&self) -> Option<&PrivateAccessProfileRuntime> {
        self.profiles.get(self.focused_index)
    }

    #[cfg(test)]
    fn focused_id(&self) -> &str {
        self.focused().id.as_str()
    }

    fn set_focus_by_id(&mut self, id: &str) -> Result<bool> {
        let Some(index) = self.profiles.iter().position(|profile| profile.id == id) else {
            bail!("unknown private access profile: {id}");
        };
        let changed = self.focused_index != index;
        self.focused_index = index;
        Ok(changed)
    }

    fn apply_state(&mut self, state: &TuiRuntimeState) -> Result<()> {
        let mut profiles = Vec::new();
        if !state.private_access_profiles.is_empty() {
            for profile_state in state.private_access_profiles.clone() {
                profiles.push(PrivateAccessProfileRuntime::from_state(profile_state)?);
            }
        }
        self.profiles = profiles;
        self.focused_index = self
            .focused_index
            .min(self.profiles.len().saturating_sub(1));
        Ok(())
    }

    fn runtime_states(&self) -> Vec<PrivateAccessProfileState> {
        self.profiles
            .iter()
            .map(PrivateAccessProfileRuntime::runtime_state)
            .collect()
    }

    #[cfg(test)]
    fn summary_line(&self) -> Option<Line<'static>> {
        let focused = self.focused_opt()?;
        let mut spans = vec![Span::styled(
            "private access: ",
            Style::default().fg(Color::DarkGray),
        )];
        for (index, profile) in self.profiles.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" "));
            }
            spans.extend(private_access_profile_badge(
                profile,
                index == self.focused_index,
            ));
        }
        if !focused.routes.is_empty() {
            spans.push(Span::styled(
                format!(" routes={}", focused.routes.len()),
                Style::default().fg(Color::Cyan),
            ));
        }
        spans.push(Span::styled(
            format!(" mode={}", focused.mode.as_str()),
            Style::default().fg(Color::DarkGray),
        ));
        if matches!(
            focused.state,
            PrivateAccessState::Connected | PrivateAccessState::Connecting
        ) {
            match focused.mode {
                PrivateAccessMode::Bridge => {
                    let bridge = focused
                        .bridge
                        .as_ref()
                        .map(|bridge| bridge.listen.as_str())
                        .unwrap_or(focused.bridge_listen.as_str());
                    spans.push(Span::styled(
                        format!(" bridge={bridge}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                PrivateAccessMode::Tun => {
                    spans.push(Span::styled(
                        " data=tun",
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
        }
        if let Some(error) = focused.last_error.as_ref() {
            spans.push(Span::raw(" error="));
            spans.push(Span::styled(
                truncate_for_width(error, 48),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(pid) = focused.background_pid {
            spans.push(Span::styled(
                format!(" pid={pid}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        Some(Line::from(spans))
    }
}

#[cfg(test)]
fn private_access_profile_badge(
    profile: &PrivateAccessProfileRuntime,
    focused: bool,
) -> Vec<Span<'static>> {
    let label = if profile.background_pid.is_some() {
        "BACKGROUND"
    } else {
        private_access_state_badge(profile.state.clone())
    };
    let state_style = private_access_state_style(&profile.state);
    let id_style = if focused {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    vec![
        Span::styled("[", Style::default().fg(Color::DarkGray)),
        Span::styled(
            if focused { ">" } else { "" },
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(profile.id.clone(), id_style),
        Span::raw(" "),
        Span::styled(label, state_style),
        Span::styled("]", Style::default().fg(Color::DarkGray)),
    ]
}

fn private_access_progress_title(profile: &PrivateAccessProfileRuntime) -> String {
    format!(
        "Private Access - {} ({})",
        profile.id,
        profile.mode.as_str()
    )
}

fn private_access_state_badge(state: PrivateAccessState) -> &'static str {
    match state {
        PrivateAccessState::Disabled => "DISABLED",
        PrivateAccessState::Disconnected => "DISCONNECTED",
        PrivateAccessState::Connecting => "CONNECTING",
        PrivateAccessState::Connected => "CONNECTED",
        PrivateAccessState::Disconnecting => "DISCONNECTING",
        PrivateAccessState::Error => "ERROR",
    }
}

fn private_access_state_style(state: &PrivateAccessState) -> Style {
    match state {
        PrivateAccessState::Connected => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        PrivateAccessState::Connecting | PrivateAccessState::Disconnecting => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        PrivateAccessState::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        PrivateAccessState::Disabled | PrivateAccessState::Disconnected => {
            Style::default().fg(Color::DarkGray)
        }
    }
}

fn should_apply_private_access_state_after_integration(
    profile: &PrivateAccessProfileRuntime,
    state: &PrivateAccessState,
) -> bool {
    !profile.integration_failed
        || matches!(
            state,
            PrivateAccessState::Error
                | PrivateAccessState::Disconnecting
                | PrivateAccessState::Disconnected
        )
}

fn private_access_profile_settings_locked(profile: &PrivateAccessProfileRuntime) -> bool {
    profile.process.is_some()
        || matches!(
            profile.state,
            PrivateAccessState::Connecting
                | PrivateAccessState::Connected
                | PrivateAccessState::Disconnecting
        )
}

fn private_access_progress_for_state(
    state: &PrivateAccessState,
    message: &str,
) -> Option<(PrivateAccessProgressTone, String, bool)> {
    let normalized = message.to_ascii_lowercase();
    // Service events are intentionally low-level, so the TUI maps them into user-facing
    // milestones. This keeps the V flow readable without leaking protocol logs into the UI.
    match state {
        PrivateAccessState::Connecting => {
            if normalized.contains("authentication accepted")
                || normalized.contains("auth accepted")
            {
                Some((
                    PrivateAccessProgressTone::Success,
                    "认证成功".to_string(),
                    false,
                ))
            } else if normalized.contains("data tunnel") || normalized.contains("tun data plane") {
                Some((
                    PrivateAccessProgressTone::Success,
                    user_private_access_message(message, "数据通道已建立"),
                    false,
                ))
            } else if normalized.contains("gateway") || normalized.contains("connecting") {
                Some((
                    PrivateAccessProgressTone::Info,
                    "正在连接内网服务器...".to_string(),
                    false,
                ))
            } else {
                Some((
                    PrivateAccessProgressTone::Info,
                    user_private_access_message(message, "正在连接内网服务器..."),
                    false,
                ))
            }
        }
        PrivateAccessState::Connected => Some((
            PrivateAccessProgressTone::Success,
            user_private_access_message(message, "连接成功"),
            false,
        )),
        PrivateAccessState::Disconnecting => Some((
            PrivateAccessProgressTone::Info,
            "正在断开内网连接...".to_string(),
            false,
        )),
        PrivateAccessState::Disconnected => Some((
            PrivateAccessProgressTone::Success,
            "内网连接已断开".to_string(),
            true,
        )),
        PrivateAccessState::Error => Some((
            PrivateAccessProgressTone::Error,
            user_private_access_message(message, "连接失败"),
            true,
        )),
        PrivateAccessState::Disabled => None,
    }
}

fn user_private_access_message(message: &str, fallback: &str) -> String {
    let message = message.trim();
    if message.is_empty() {
        fallback.to_string()
    } else {
        message.to_string()
    }
}

fn load_private_access_manifest_for_profile(
    profile_id: &str,
    manifest_path: Option<&str>,
) -> Result<PrivateAccessServiceManifest> {
    if let Some(path) = manifest_path.filter(|path| !path.trim().is_empty()) {
        return load_private_access_manifest(Path::new(path));
    }
    if profile_id.eq_ignore_ascii_case("sonicwall") {
        default_sonicwall_manifest()
    } else {
        default_hillstone_manifest()
    }
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
    fn new(
        client: ApiClient,
        benchmark_max_concurrency: usize,
        subscription_refresh_options: TuiSubscriptionRefreshOptions,
        keep_sing_box_running: bool,
        manage_sing_box: bool,
    ) -> Result<Self> {
        let state_store = TuiStateStore::new(default_tui_state_path());
        let existing_state_file = state_store.exists();
        let runtime_state = state_store.load()?;
        let onboarding_complete = runtime_state.onboarding_complete || existing_state_file;
        let system_proxy_server =
            default_system_proxy_server(&subscription_refresh_options.config_path);
        let system_proxy_enabled = system_proxy_matches(&system_proxy_server);
        let system_proxy_config_path = subscription_refresh_options.config_path.clone();
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
            last_background_status_refresh: Instant::now() - BACKGROUND_STATUS_REFRESH_INTERVAL,
            last_background_status_generation: 0,
            background_started_at_unix: current_unix_timestamp(),
            background_worker: None,
            background_status_job: None,
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
            system_proxy_config_path,
            system_proxy_server,
            system_proxy_server_override: false,
            system_proxy_enabled,
            system_proxy_job: None,
            last_system_proxy_status_refresh: Instant::now() - SYSTEM_PROXY_STATUS_REFRESH_INTERVAL,
            verify_job: None,
            sing_box: SingBoxProcessRuntime::new(keep_sing_box_running),
            private_access: PrivateAccessRuntime::new()?,
            private_access_progress: None,
            private_access_auth: None,
        };
        app.apply_runtime_state(runtime_state.clone())?;
        if manage_sing_box {
            app.ensure_private_access_carrier_routes()?;
            app.start_managed_sing_box()?;
            if let Err(error) = app.wait_for_controller_ready() {
                let _ = app.shutdown_managed_sing_box();
                return Err(error);
            }
        } else {
            app.wait_for_controller_ready()
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

    fn ensure_private_access_carrier_routes(&self) -> Result<bool> {
        if !self.system_proxy_config_path.exists() {
            return Ok(false);
        }
        let carrier_domains = self
            .private_access
            .profiles
            .iter()
            .filter(|profile| profile.manifest.id.eq_ignore_ascii_case("sonicwall"))
            .map(|profile| profile.server.trim().to_ascii_lowercase())
            .filter(|server| !server.is_empty())
            .collect::<Vec<_>>();
        run_private_access_carrier_route_config(
            &self.system_proxy_config_path,
            true,
            &carrier_domains,
        )
    }

    fn apply_runtime_state(&mut self, state: TuiRuntimeState) -> Result<()> {
        self.private_access.apply_state(&state)?;
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
            self.system_proxy_server = value;
            self.system_proxy_server_override = state.system_proxy_server_override;
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

    fn apply_background_auto_pick_config(&mut self, config: BackgroundAutoPickConfig) {
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
            system_proxy_server: Some(self.system_proxy_server.clone()),
            system_proxy_server_override: self.system_proxy_server_override,
            private_access_profiles: self.private_access.runtime_states(),
        }
    }

    fn background_auto_pick_config(&self) -> BackgroundAutoPickConfig {
        BackgroundAutoPickConfig {
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
        match (self.sing_box.managed_pid, self.sing_box.keep_running) {
            (Some(pid), true) => format!("sing-box: managed pid={pid} exit=keep-background"),
            (Some(pid), false) => format!("sing-box: managed pid={pid} exit=stop"),
            (None, true) => "sing-box: not managed exit=keep-background".to_string(),
            (None, false) => "sing-box: not managed exit=stop".to_string(),
        }
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
            "connections active={} proxy={} direct={} up={} down={}  c details",
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

    fn current_background_status_target(&self) -> Result<Option<BackgroundStatusTarget>> {
        if let Some(worker) = self.background_worker.as_ref() {
            return Ok(Some(BackgroundStatusTarget {
                pid: worker.pid,
                bind_addr: worker.bind_addr.clone(),
                token: worker.token.clone(),
            }));
        }
        Ok(
            read_background_task_state()?.map(|state| BackgroundStatusTarget {
                pid: state.pid,
                bind_addr: state.bind_addr,
                token: state.token,
            }),
        )
    }

    fn clear_failed_background_status_target(
        &mut self,
        target: &BackgroundStatusTarget,
    ) -> Result<()> {
        if self
            .background_worker
            .as_ref()
            .is_some_and(|worker| worker.pid == target.pid)
        {
            let mut worker = self.background_worker.take().expect("worker exists");
            if let Some(child) = worker.child.as_mut() {
                let _ = child.try_wait();
            }
        }
        if read_background_task_state()?.is_some_and(|state| state.pid == target.pid) {
            remove_background_task_state_file();
        }
        Ok(())
    }

    fn apply_background_status_snapshot(
        &mut self,
        snapshot: BackgroundStatusSnapshot,
    ) -> Result<()> {
        self.apply_background_latency_snapshot(snapshot.latency.as_ref());
        if snapshot.status_generation > self.last_background_status_generation {
            self.last_background_status_generation = snapshot.status_generation;
            let status = snapshot.worker_status;
            if background_status_requires_selector_refresh(&status) {
                self.refresh()?;
            }
            self.set_status_only(format!("Auto-pick worker: {status}"));
        }
        Ok(())
    }

    fn poll_background_auto_pick_status(&mut self) -> Result<()> {
        if !self.background_worker_management_enabled() {
            return Ok(());
        }

        if let Some(job) = self.background_status_job.as_ref() {
            let outcome = match job.receiver.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(BackgroundStatusPollOutcome {
                    result: Err("background status poll thread disconnected".to_string()),
                    // Do not create a replacement from an ambiguous polling failure. The next
                    // poll can retry without risking a second live worker.
                    process_alive: true,
                }),
            };
            if let Some(outcome) = outcome {
                let job = self
                    .background_status_job
                    .take()
                    .expect("background status job exists");
                let target = job.target;
                let _ = job.worker.join();
                if self.current_background_status_target()?.as_ref() == Some(&target) {
                    match resolve_background_status_poll(outcome) {
                        BackgroundStatusPollResolution::Snapshot(snapshot) => {
                            if self.background_worker.is_none() {
                                self.background_worker = Some(BackgroundWorkerRuntime {
                                    pid: target.pid,
                                    bind_addr: target.bind_addr.clone(),
                                    token: target.token.clone(),
                                    child: None,
                                });
                            }
                            self.apply_background_status_snapshot(*snapshot)?;
                        }
                        BackgroundStatusPollResolution::Retry(error) => {
                            self.set_status_only(format!(
                                "Auto-pick worker TCP error; process is still alive, retrying: {error}"
                            ));
                        }
                        BackgroundStatusPollResolution::Reconnect(error) => {
                            self.set_status_only(format!(
                                "Auto-pick worker exited after TCP error: {error}"
                            ));
                            self.clear_failed_background_status_target(&target)?;
                            if self.auto_select_enabled {
                                let worker = self.ensure_auto_pick_background_worker()?;
                                self.set_status_only(format!(
                                    "Auto-pick background worker {} pid {} after previous worker exited",
                                    worker.label(),
                                    worker.pid()
                                ));
                            }
                        }
                    }
                }
            }
        }

        if self.background_status_job.is_some()
            || self.last_background_status_refresh.elapsed() < BACKGROUND_STATUS_REFRESH_INTERVAL
        {
            return Ok(());
        }

        if let Some(target) = self.current_background_status_target()? {
            self.last_background_status_refresh = Instant::now();
            self.background_status_job = Some(spawn_background_status_poll(target));
        } else if self.auto_select_enabled {
            let worker = self.ensure_auto_pick_background_worker()?;
            self.set_status_only(format!(
                "Auto-pick background worker {} pid {}",
                worker.label(),
                worker.pid()
            ));
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
                KeyCode::Char('G') => self.help_index = HELP_BINDINGS.len().saturating_sub(1),
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

    fn handle_private_access_auth_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc => self.cancel_private_access_auth()?,
            KeyCode::Tab | KeyCode::Down => {
                if let Some(auth) = self.private_access_auth.as_mut() {
                    auth.field_index =
                        (auth.field_index + 1).min(auth.fields.len().saturating_sub(1));
                    auth.error = None;
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(auth) = self.private_access_auth.as_mut() {
                    auth.field_index = auth.field_index.saturating_sub(1);
                    auth.error = None;
                }
            }
            KeyCode::Left | KeyCode::Right => {
                if let Some(auth) = self.private_access_auth.as_mut()
                    && let Some(field) = auth.fields.get(auth.field_index)
                    && !field.options.is_empty()
                {
                    let current = auth.inputs[auth.field_index].as_str();
                    let current_index = field
                        .options
                        .iter()
                        .position(|option| option.value == current)
                        .unwrap_or(0);
                    let next = if matches!(code, KeyCode::Left) {
                        current_index.saturating_sub(1)
                    } else {
                        (current_index + 1).min(field.options.len() - 1)
                    };
                    auth.inputs[auth.field_index] = field.options[next].value.clone();
                    auth.error = None;
                }
            }
            KeyCode::Enter => {
                let submit = self
                    .private_access_auth
                    .as_ref()
                    .is_some_and(|auth| auth.field_index + 1 >= auth.fields.len());
                if submit {
                    self.submit_private_access_auth()?;
                } else if let Some(auth) = self.private_access_auth.as_mut() {
                    auth.field_index += 1;
                    auth.error = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(auth) = self.private_access_auth.as_mut()
                    && let Some(input) = auth.inputs.get_mut(auth.field_index)
                {
                    input.pop();
                    auth.error = None;
                }
            }
            KeyCode::Char(ch) => {
                if let Some(auth) = self.private_access_auth.as_mut()
                    && let Some(input) = auth.inputs.get_mut(auth.field_index)
                {
                    input.push(ch);
                    auth.error = None;
                }
            }
            _ => {}
        }
        Ok(true)
    }

    fn submit_private_access_auth(&mut self) -> Result<()> {
        let Some(auth) = self.private_access_auth.as_mut() else {
            return Ok(());
        };
        if let Some((index, field)) = auth
            .fields
            .iter()
            .enumerate()
            .find(|(index, field)| field.required && auth.inputs[*index].trim().is_empty())
        {
            auth.field_index = index;
            auth.error = Some(format!("{} is required", field.label));
            return Ok(());
        }

        let mut auth = self
            .private_access_auth
            .take()
            .expect("private access auth modal exists");
        let inputs = std::mem::take(&mut auth.inputs);
        let replies = inputs
            .into_iter()
            .map(PrivateAccessSecret::new)
            .collect::<Vec<_>>();
        let button = auth
            .buttons
            .iter()
            .find(|button| button.eq_ignore_ascii_case("ok"))
            .or_else(|| {
                auth.buttons
                    .iter()
                    .find(|button| !button.eq_ignore_ascii_case("cancel"))
            })
            .cloned()
            .unwrap_or_else(|| "ok".to_string());
        let command = PrivateAccessCommand::AuthReply {
            id: "tui-auth-reply".to_string(),
            service: auth.service.clone(),
            session_id: auth.session_id.clone(),
            challenge_id: auth.challenge_id.clone(),
            button,
            replies,
        };
        let Some(process) = self
            .private_access
            .profiles
            .get_mut(auth.profile_index)
            .and_then(|profile| profile.process.as_mut())
        else {
            self.set_status_only("Private Access authentication session closed");
            return Ok(());
        };
        if let Err(error) = process.send(&command) {
            self.set_status_only(format!(
                "Failed to submit Private Access authentication: {error}"
            ));
            return Ok(());
        }
        self.set_status_only("Private Access authentication submitted");
        Ok(())
    }

    fn cancel_private_access_auth(&mut self) -> Result<()> {
        let Some(auth) = self.private_access_auth.take() else {
            return Ok(());
        };
        let command = PrivateAccessCommand::CancelAuth {
            id: "tui-auth-cancel".to_string(),
            service: auth.service.clone(),
            session_id: auth.session_id.clone(),
            challenge_id: auth.challenge_id.clone(),
        };
        if let Some(process) = self
            .private_access
            .profiles
            .get_mut(auth.profile_index)
            .and_then(|profile| profile.process.as_mut())
        {
            process.send(&command)?;
        }
        self.set_status_only("Private Access authentication cancelled");
        Ok(())
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
                self.system_proxy_server = value.to_string();
                self.system_proxy_server_override = true;
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
                let manifest = load_private_access_manifest_for_profile(
                    &profile_id,
                    manifest_path.as_deref(),
                )?;
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
        self.help_index = (self.help_index + 1).min(HELP_BINDINGS.len().saturating_sub(1));
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
            if matches!(worker, BackgroundTaskEnsureResult::AlreadyRunning(_)) {
                self.send_background_auto_pick_config()?;
            }
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
        if !self.auto_select_enabled {
            return false;
        }
        self.last_auto_select_benchmark
            .is_none_or(|last| now.duration_since(last) >= self.auto_select_interval)
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
        if self.benchmark_filter.is_empty() {
            "all nodes".to_string()
        } else {
            format!("filter '{}'", self.benchmark_filter)
        }
    }

    fn auto_select_group(&self) -> Option<&ProxyGroup> {
        self.auto_select_selector
            .as_deref()
            .and_then(|selector| self.group_by_name(selector))
            .or_else(|| self.selected_group())
    }

    fn auto_select_target(&self, group: &ProxyGroup, summary: &BenchmarkSummary) -> Option<String> {
        let best = summary.best_success_matching_filter()?;
        let current = group.current.as_deref();
        let current_matches_filter =
            current.is_some_and(|name| matches_filter(name, &summary.pattern));
        let current_result = current.and_then(|name| summary.find_result(name));
        let current_is_acceptable = current_matches_filter
            && current_result
                .and_then(|result| result.delay)
                .is_some_and(|delay| delay <= self.auto_select_threshold_ms);
        if current_is_acceptable {
            return None;
        }
        if current == Some(best.name.as_str()) {
            return None;
        }
        Some(best.name.clone())
    }

    fn auto_select_switch_plan(
        &self,
        group: &ProxyGroup,
        summary: &BenchmarkSummary,
    ) -> AutoSelectSwitchPlan {
        AutoSelectSwitchPlan {
            target_node: self.auto_select_target(group, summary),
            parent_switch: self.implicit_root_parent_switch_for_group(&group.name),
        }
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
        if !self.system_proxy_server_override {
            self.system_proxy_server = default_system_proxy_server(&self.system_proxy_config_path);
        }
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
        let proxy_server = self.system_proxy_server.clone();
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

    fn private_access_connect_needs_terminal_prompt(&self) -> bool {
        let Some(profile) = self.private_access.focused_opt() else {
            return false;
        };
        matches!(
            profile.state,
            PrivateAccessState::Disabled
                | PrivateAccessState::Disconnected
                | PrivateAccessState::Error
        ) && matches!(profile.mode, PrivateAccessMode::Tun)
            && self
                .private_access_tun_helper_for_connect(profile)
                .is_some_and(|command| helper_command_uses_interactive_sudo(&command))
    }

    fn private_access_tun_helper_for_connect(
        &self,
        profile: &PrivateAccessProfileRuntime,
    ) -> Option<Vec<String>> {
        if !matches!(profile.mode, PrivateAccessMode::Tun) {
            return None;
        }
        if !profile.tun_helper.is_empty() {
            return Some(profile.tun_helper.clone());
        }
        // TUN device setup may need a privileged helper. The TUI injects it only for the live
        // connect command, so the user gets a clear prompt without persisting machine-specific
        // helper paths into sing-box-tui.json.
        Some(default_tui_tun_helper_command())
    }

    fn start_managed_sing_box(&mut self) -> Result<()> {
        let result = self.restart_managed_sing_box()?;
        if result.restarted_pids.is_empty() {
            self.status = format!("Started managed sing-box pid {}", result.started_pid);
        } else {
            self.status = format!(
                "Restarted managed sing-box pid(s) {:?} -> {}",
                result.restarted_pids, result.started_pid
            );
        }
        Ok(())
    }

    fn restart_managed_sing_box(&mut self) -> Result<SingBoxRestartSummary> {
        self.stop_managed_sing_box_process()?;
        let result = restart_sing_box_for_config(&self.system_proxy_config_path)?;
        let summary = SingBoxRestartSummary {
            restarted_pids: result.restarted_pids,
            started_pid: result.started_pid,
        };
        self.sing_box.managed_pid = Some(result.started_pid);
        self.sing_box.managed_child = Some(result.child);
        Ok(summary)
    }

    fn wait_for_controller_ready(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut last_error = None;
        while Instant::now() < deadline {
            match self.client.fetch_config() {
                Ok(_) => return Ok(()),
                Err(error) => {
                    last_error = Some(error);
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
        match last_error {
            Some(error) => {
                Err(error).context("sing-box started but controller API did not become ready")
            }
            None => bail!("sing-box started but controller API did not become ready"),
        }
    }

    fn shutdown_managed_sing_box(&mut self) -> Result<()> {
        if self.sing_box.keep_running {
            return Ok(());
        }
        if self.background_worker_management_enabled() {
            self.stop_live_background_auto_pick_task()?;
        }
        self.stop_managed_sing_box_process().map(|_| ())
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
        self.sing_box.keep_running = true;
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

    fn detach_private_access_for_background(&mut self) -> Result<usize> {
        let mut detached = 0;
        for profile in &mut self.private_access.profiles {
            if profile.background_pid.is_some_and(process_exists) {
                detached += 1;
                continue;
            }
            let Some(process) = profile.process.as_mut() else {
                continue;
            };
            let pid = process.pid();
            process.detach().with_context(|| {
                format!("failed to detach Private Access profile {}", profile.id)
            })?;
            profile.background_pid = Some(pid);
            profile.state = PrivateAccessState::Connected;
            profile.last_error = None;
            detached += 1;
        }
        Ok(detached)
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
        if self.auto_select_enabled {
            if self.background_worker_management_enabled() {
                let worker = self.ensure_auto_pick_background_worker()?;
                if matches!(worker, BackgroundTaskEnsureResult::AlreadyRunning(_)) {
                    self.send_background_auto_pick_config()?;
                }
            }
        }
        Ok(())
    }

    fn background_worker_management_enabled(&self) -> bool {
        self.state_store.is_some() && !cfg!(test)
    }

    fn ensure_auto_pick_background_worker(&mut self) -> Result<BackgroundTaskEnsureResult> {
        let config = self.background_auto_pick_config();
        if let Some(worker) = self.background_worker.as_ref() {
            match send_background_control_request(
                &worker.bind_addr,
                &worker.token,
                BackgroundWorkerCommand::ApplyConfig {
                    config: config.clone(),
                },
            ) {
                Ok(_) => return Ok(BackgroundTaskEnsureResult::AlreadyRunning(worker.pid)),
                Err(error) if process_exists(worker.pid) => {
                    return Err(error).with_context(|| {
                        format!(
                            "background auto-pick worker {} is alive but its control channel is unavailable",
                            worker.pid
                        )
                    });
                }
                Err(_) => {}
            }
            self.background_worker = None;
        }
        if let Some(state) = read_background_task_state()? {
            let bind_addr = state.bind_addr.clone();
            let token = state.token.clone();
            match send_background_control_request(
                &bind_addr,
                &token,
                BackgroundWorkerCommand::ApplyConfig {
                    config: config.clone(),
                },
            ) {
                Ok(_) => {
                    self.background_worker = Some(BackgroundWorkerRuntime {
                        pid: state.pid,
                        bind_addr,
                        token,
                        child: None,
                    });
                    return Ok(BackgroundTaskEnsureResult::AlreadyRunning(state.pid));
                }
                Err(error) if process_exists(state.pid) => {
                    return Err(error).with_context(|| {
                        format!(
                            "registered background auto-pick worker {} is alive but its control channel is unavailable",
                            state.pid
                        )
                    });
                }
                Err(_) => {
                    remove_background_task_state_file();
                }
            }
        }
        self.spawn_headless_auto_pick_process()
            .map(BackgroundTaskEnsureResult::Started)
    }

    fn spawn_headless_auto_pick_process(&mut self) -> Result<u32> {
        let exe = env::current_exe().context("failed to locate current executable")?;
        let log_path = background_task_log_path();
        if let Some(parent) = log_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create background worker log directory {}",
                    parent.display()
                )
            })?;
        }
        let stderr = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .with_context(|| {
                format!(
                    "failed to open background worker log {}",
                    log_path.display()
                )
            })?;
        let mut command = Command::new(exe);
        command
            .arg("run")
            .arg("--headless-auto-pick")
            .arg("--controller")
            .arg(self.client.base_url.as_str())
            .arg("--max-concurrency")
            .arg(self.benchmark_max_concurrency.to_string())
            .arg("--config")
            .arg(&self.system_proxy_config_path)
            .arg("--no-subscription-refresh")
            .env("SING_BOX_TUI_BACKGROUND_TOKEN", random_background_token())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        let mut child = command
            .spawn()
            .context("failed to spawn headless auto-pick process")?;
        let pid = child.id();
        let state = match wait_for_background_registry(&mut child, &log_path) {
            Ok(state) => state,
            Err(error) => {
                let _ = child.kill();
                return Err(error).context("background auto-pick worker did not initialize");
            }
        };
        let bind_addr = state.bind_addr.clone();
        let token = state.token.clone();
        send_background_control_request(
            &bind_addr,
            &token,
            BackgroundWorkerCommand::ApplyConfig {
                config: self.background_auto_pick_config(),
            },
        )
        .with_context(|| {
            format!(
                "failed to apply initial background auto-pick config{}",
                background_log_tail_context(&log_path)
            )
        })?;
        self.background_worker = Some(BackgroundWorkerRuntime {
            pid,
            bind_addr,
            token,
            child: Some(child),
        });
        Ok(pid)
    }

    fn send_background_auto_pick_config(&mut self) -> Result<()> {
        let config = self.background_auto_pick_config();
        if let Some(worker) = self.background_worker.as_ref() {
            send_background_control_request(
                &worker.bind_addr,
                &worker.token,
                BackgroundWorkerCommand::ApplyConfig { config },
            )?;
        }
        Ok(())
    }

    fn stop_live_background_auto_pick_task(&mut self) -> Result<()> {
        let Some(mut worker) = self.background_worker.take() else {
            return stop_background_auto_pick_task();
        };
        let _ = send_background_control_request(
            &worker.bind_addr,
            &worker.token,
            BackgroundWorkerCommand::Stop,
        );
        if wait_for_background_process_to_exit(worker.pid, Duration::from_secs(3)).is_err() {
            if let Some(mut child) = worker.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            } else {
                let _ = stop_background_pid(worker.pid);
            }
        } else if let Some(mut child) = worker.child.take() {
            let _ = child.wait();
        }
        remove_background_task_state_file();
        Ok(())
    }

    fn background_status_snapshot(
        &self,
        worker_status: String,
        generation: u64,
    ) -> BackgroundStatusSnapshot {
        BackgroundStatusSnapshot {
            kind: BACKGROUND_TASK_KIND_AUTO_PICK.to_string(),
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
        let token = background_token_from_env();
        let (bind_addr, commands) =
            spawn_background_tcp_server(&background_bind_addr(), token.clone())?;
        write_background_task_state(&BackgroundTaskState {
            version: 2,
            kind: BACKGROUND_TASK_KIND_AUTO_PICK.to_string(),
            pid: std::process::id(),
            controller: self.client.base_url.clone(),
            config_path: self.system_proxy_config_path.clone(),
            max_concurrency: self.benchmark_max_concurrency,
            started_at_unix: self.background_started_at_unix,
            status_generation: 0,
            status: Some("starting".to_string()),
            updated_at_unix: Some(current_unix_timestamp()),
            bind_addr: bind_addr.to_string(),
            token,
        })?;
        self.auto_select_enabled = false;
        let mut last_published_status = String::new();
        let mut status_generation = 0;
        loop {
            loop {
                match commands.try_recv() {
                    Ok(request) => match request.command {
                        BackgroundWorkerCommand::Status => {
                            let _ = request.response.send(BackgroundControlResponse {
                                ok: true,
                                error: None,
                                status: Some(self.background_status_snapshot(
                                    self.status.clone(),
                                    status_generation,
                                )),
                            });
                        }
                        BackgroundWorkerCommand::ApplyConfig { config } => {
                            self.apply_background_auto_pick_config(config);
                            last_published_status.clear();
                            status_generation = status_generation.saturating_add(1);
                            let _ = request.response.send(BackgroundControlResponse {
                                ok: true,
                                error: None,
                                status: Some(self.background_status_snapshot(
                                    "configuration applied".to_string(),
                                    status_generation,
                                )),
                            });
                        }
                        BackgroundWorkerCommand::Stop => {
                            status_generation = status_generation.saturating_add(1);
                            let _ = request.response.send(BackgroundControlResponse {
                                ok: true,
                                error: None,
                                status: Some(self.background_status_snapshot(
                                    "stopping".to_string(),
                                    status_generation,
                                )),
                            });
                            remove_background_task_state_file();
                            return Ok(());
                        }
                    },
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
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

    fn stop_managed_sing_box_process(&mut self) -> Result<Option<u32>> {
        let pid = self.sing_box.managed_pid.take();
        if let Some(mut child) = self.sing_box.managed_child.take() {
            let child_pid = child.id();
            stop_sing_box_child(&mut child)
                .with_context(|| format!("failed to stop managed sing-box pid {child_pid}"))?;
            return Ok(Some(child_pid));
        }
        if let Some(pid) = pid {
            stop_sing_box_pid(pid)
                .with_context(|| format!("failed to stop managed sing-box pid {pid}"))?;
            return Ok(Some(pid));
        }
        Ok(None)
    }

    fn open_private_access_progress(&mut self) {
        self.open_private_access_progress_for_profile(self.private_access.focused_index);
    }

    fn open_private_access_progress_for_profile(&mut self, profile_index: usize) {
        let Some(profile) = self.private_access.profiles.get(profile_index) else {
            return;
        };
        self.private_access_progress = Some(PrivateAccessProgressModal {
            profile_index,
            title: private_access_progress_title(profile),
            entries: Vec::new(),
            done: false,
        });
        self.flash = None;
    }

    fn push_private_access_progress(&mut self, tone: PrivateAccessProgressTone, text: String) {
        self.push_private_access_progress_for_profile(
            self.private_access.focused_index,
            tone,
            text,
        );
    }

    fn push_private_access_progress_for_profile(
        &mut self,
        profile_index: usize,
        tone: PrivateAccessProgressTone,
        text: String,
    ) {
        if !matches!(
            self.private_access_progress.as_ref(),
            Some(progress) if progress.profile_index == profile_index
        ) {
            self.open_private_access_progress_for_profile(profile_index);
        }
        let Some(progress) = self.private_access_progress.as_mut() else {
            return;
        };
        if progress.profile_index != profile_index {
            return;
        }
        if progress
            .entries
            .last()
            .is_some_and(|entry| entry.tone == tone && entry.text == text)
        {
            return;
        }
        progress
            .entries
            .push(PrivateAccessProgressEntry { tone, text });
    }

    fn finish_private_access_progress(&mut self) {
        self.finish_private_access_progress_for_profile(self.private_access.focused_index);
    }

    fn finish_private_access_progress_for_profile(&mut self, profile_index: usize) {
        if let Some(progress) = self.private_access_progress.as_mut() {
            if progress.profile_index == profile_index {
                progress.done = true;
            }
        }
    }

    fn fail_private_access_progress(&mut self, message: String) {
        self.fail_private_access_progress_for_profile(self.private_access.focused_index, message);
    }

    fn fail_private_access_progress_for_profile(&mut self, profile_index: usize, message: String) {
        self.push_private_access_progress_for_profile(
            profile_index,
            PrivateAccessProgressTone::Error,
            message,
        );
        self.finish_private_access_progress_for_profile(profile_index);
    }

    fn toggle_private_access_with_progress(&mut self) -> Result<()> {
        if !self.private_access.is_configured() {
            self.set_status_only("Private Access is not configured");
            return Ok(());
        }
        self.open_private_access_progress();
        match self.private_access.focused().state {
            PrivateAccessState::Connected | PrivateAccessState::Connecting => {
                self.push_private_access_progress(
                    PrivateAccessProgressTone::Info,
                    "正在断开内网连接...".to_string(),
                );
            }
            PrivateAccessState::Disconnecting => {
                self.push_private_access_progress(
                    PrivateAccessProgressTone::Info,
                    "正在等待断开完成...".to_string(),
                );
            }
            PrivateAccessState::Disabled
            | PrivateAccessState::Disconnected
            | PrivateAccessState::Error => {
                self.push_private_access_progress(
                    PrivateAccessProgressTone::Info,
                    "正在连接内网服务器...".to_string(),
                );
            }
        }
        self.toggle_private_access()
    }

    fn toggle_private_access(&mut self) -> Result<()> {
        if !self.private_access.is_configured() {
            self.set_status_only("Private Access is not configured");
            return Ok(());
        }
        match self.private_access.focused().state {
            PrivateAccessState::Connected | PrivateAccessState::Connecting => {
                self.disconnect_private_access()
            }
            PrivateAccessState::Disconnecting => {
                self.set_status_only("Private Access disconnect is already running");
                Ok(())
            }
            PrivateAccessState::Disabled
            | PrivateAccessState::Disconnected
            | PrivateAccessState::Error => self.connect_private_access(),
        }
    }

    fn connect_private_access(&mut self) -> Result<()> {
        if !self.private_access.is_configured() {
            self.set_status_only("Private Access is not configured");
            return Ok(());
        }
        if self.private_access.focused().server.trim().is_empty() {
            let message = "请先在 settings 中配置 Private Access server".to_string();
            self.fail_private_access_progress(message.clone());
            self.set_status_only(message);
            return Ok(());
        }
        if self.private_access.focused().manifest.id != "sonicwall"
            && self.private_access.focused().username.trim().is_empty()
        {
            let message = "请先在 settings 中配置 Private Access username".to_string();
            self.fail_private_access_progress(message.clone());
            self.set_status_only(message);
            return Ok(());
        }
        if matches!(
            self.private_access.focused().mode,
            PrivateAccessMode::Bridge
        ) {
            if let Err(error) = self
                .private_access
                .focused()
                .bridge_listen
                .parse::<SocketAddrV4>()
            {
                let message = format!("Private Access bridge listen 无效: {error}");
                self.fail_private_access_progress(message.clone());
                self.set_status_only(message);
                return Ok(());
            }
        }
        if self.private_access.focused().manifest.id == "sonicwall" {
            let official_processes = running_official_sonicwall_client_processes();
            if !official_processes.is_empty() {
                let message = format_official_sonicwall_client_warning(&official_processes);
                self.fail_private_access_progress(message.clone());
                self.set_status_only(message);
                return Ok(());
            }
        }

        if self.private_access.focused().process.is_none() {
            match ExternalPrivateAccessService::spawn(
                self.private_access.focused().manifest.clone(),
            ) {
                Ok(process) => {
                    self.private_access.focused_mut().process = Some(process);
                }
                Err(error) => {
                    self.private_access.focused_mut().state = PrivateAccessState::Error;
                    self.private_access.focused_mut().last_error = Some(error.to_string());
                    let message = format!("启动 Private Access service 失败: {error}");
                    self.fail_private_access_progress(message.clone());
                    self.set_status_only(message);
                    return Ok(());
                }
            }
        }
        let profile_id = self.private_access.focused().id.clone();
        let service = self.private_access.focused().manifest.id.clone();
        let (password, password_env) = if service == "sonicwall" {
            (None, None)
        } else {
            (
                normalize_optional_setting(Some(self.private_access.focused().password.clone())),
                normalize_optional_setting(Some(
                    self.private_access.focused().password_env.clone(),
                )),
            )
        };
        let tun_helper = self.private_access_tun_helper_for_connect(self.private_access.focused());
        let http_connect_proxy = (service == "sonicwall")
            .then(|| normalize_http_connect_proxy(&self.system_proxy_server))
            .flatten();
        let http_connect_proxy_context = (service == "sonicwall")
            .then(|| self.internet_outbound_context())
            .flatten();
        let http_connect_controller =
            (service == "sonicwall").then(|| self.client.base_url.clone());
        let http_connect_selector = (service == "sonicwall")
            .then(|| self.internet_outbound_root_selector())
            .flatten();
        // Direct passwords are deliberately supported for a simpler local workflow. The value is
        // sent only to the service process; the settings list displays the configured value.
        let command = PrivateAccessCommand::Connect {
            id: "tui-connect".to_string(),
            service: service.clone(),
            config: serde_json::json!({
                "server": self.private_access.focused().server,
                "mode": self.private_access.focused().mode.as_str(),
                "port": self.private_access.focused().port,
                "username": self.private_access.focused().username,
                "password": password,
                "password_env": password_env,
                "bridge_listen": self.private_access.focused().bridge_listen,
                "tun_helper": tun_helper,
                "http_connect_proxy": http_connect_proxy,
                "http_connect_proxy_context": http_connect_proxy_context,
                "http_connect_controller": http_connect_controller,
                "http_connect_selector": http_connect_selector,
                "tls_verify": self.private_access.focused().tls_verify,
            }),
        };
        if let Some(process) = self.private_access.focused_mut().process.as_mut() {
            if let Err(error) = process.send(&command) {
                self.private_access.focused_mut().state = PrivateAccessState::Error;
                self.private_access.focused_mut().last_error = Some(error.to_string());
                let message = format!("发送 Private Access 连接命令失败: {error}");
                self.fail_private_access_progress(message.clone());
                self.set_status_only(message);
                return Ok(());
            }
        }
        self.private_access.focused_mut().state = PrivateAccessState::Connecting;
        self.private_access.focused_mut().last_error = None;
        self.private_access.focused_mut().integration_failed = false;
        self.private_access.focused_mut().background_pid = None;
        self.set_status_only(format!(
            "Private Access {profile_id} ({service}) connecting..."
        ));
        self.save_runtime_state()?;
        Ok(())
    }

    fn disconnect_private_access(&mut self) -> Result<()> {
        if !self.private_access.is_configured() {
            self.set_status_only("Private Access is not configured");
            return Ok(());
        }
        if self.private_access.focused().process.is_none()
            && let Some(pid) = self.private_access.focused().background_pid
        {
            let message = format!(
                "Private Access is running in background pid {pid}; this TUI no longer owns its service session"
            );
            self.push_private_access_progress(PrivateAccessProgressTone::Info, message.clone());
            self.finish_private_access_progress();
            self.set_status_only(message);
            return Ok(());
        }
        let Some(process) = self.private_access.focused_mut().process.as_mut() else {
            self.private_access.focused_mut().state = PrivateAccessState::Disconnected;
            self.push_private_access_progress(
                PrivateAccessProgressTone::Success,
                "内网连接已断开".to_string(),
            );
            self.finish_private_access_progress();
            self.set_status_only("Private Access is already disconnected");
            return Ok(());
        };
        let service = process.service_id().to_string();
        if let Err(error) = process.send(&PrivateAccessCommand::Disconnect {
            id: "tui-disconnect".to_string(),
            service: service.clone(),
            session_id: None,
        }) {
            self.private_access.focused_mut().state = PrivateAccessState::Error;
            self.private_access.focused_mut().last_error = Some(error.to_string());
            let message = format!("发送 Private Access 断开命令失败: {error}");
            self.fail_private_access_progress(message.clone());
            self.set_status_only(message);
            return Ok(());
        }
        self.private_access.focused_mut().state = PrivateAccessState::Disconnecting;
        let profile_id = self.private_access.focused().id.clone();
        self.set_status_only(format!(
            "Private Access {profile_id} ({service}) disconnecting..."
        ));
        Ok(())
    }

    fn poll_private_access_updates(&mut self) -> Result<()> {
        for profile_index in 0..self.private_access.profiles.len() {
            let mut stop_process = false;
            // Keep protocol chatter from monopolizing the TUI loop. Any remaining events stay in
            // the channel for the next frame, so keyboard input is serviced between batches.
            for _ in 0..PRIVATE_ACCESS_EVENTS_PER_POLL {
                let profile_id = self.private_access.profiles[profile_index].id.clone();
                let event = match self.private_access.profiles[profile_index].process.as_ref() {
                    Some(process) => match process.try_recv() {
                        Ok(Some(event)) => event,
                        Ok(None) => break,
                        Err(error) => {
                            self.private_access.profiles[profile_index].last_error =
                                Some(error.clone());
                            self.private_access.profiles[profile_index].state =
                                PrivateAccessState::Error;
                            self.set_status_with_flash(format!(
                                "Private Access {profile_id} failed: {error}"
                            ));
                            stop_process = true;
                            break;
                        }
                    },
                    None => break,
                };
                match event.event {
                    PrivateAccessEvent::StateChanged {
                        service,
                        state,
                        message,
                    } => {
                        if !should_apply_private_access_state_after_integration(
                            &self.private_access.profiles[profile_index],
                            &state,
                        ) {
                            continue;
                        }
                        self.private_access.profiles[profile_index].state = state.clone();
                        if matches!(state, PrivateAccessState::Disconnected) {
                            if self
                                .private_access_auth
                                .as_ref()
                                .is_some_and(|auth| auth.profile_index == profile_index)
                            {
                                self.private_access_auth = None;
                            }
                            stop_process = true;
                        }
                        if let Some((tone, text, done)) =
                            private_access_progress_for_state(&state, &message)
                        {
                            self.push_private_access_progress_for_profile(
                                profile_index,
                                tone,
                                text,
                            );
                            if done {
                                self.finish_private_access_progress_for_profile(profile_index);
                            }
                        }
                        self.set_status_only(format!(
                            "Private Access {profile_id} ({service}) {}",
                            state.label()
                        ));
                    }
                    PrivateAccessEvent::RoutesPushed {
                        service,
                        routes,
                        dns,
                        domains,
                        domain_suffixes,
                        bridge,
                        ..
                    } => {
                        self.push_private_access_progress_for_profile(
                            profile_index,
                            PrivateAccessProgressTone::Info,
                            format!("收到内网路由: {} 条", routes.len()),
                        );
                        self.push_private_access_progress_for_profile(
                            profile_index,
                            PrivateAccessProgressTone::Info,
                            "修改 config.json 中...".to_string(),
                        );
                        self.private_access.profiles[profile_index].routes = routes.clone();
                        self.private_access.profiles[profile_index].dns = dns;
                        self.private_access.profiles[profile_index].domains = domains.clone();
                        self.private_access.profiles[profile_index].domain_suffixes =
                            domain_suffixes.clone();
                        let carrier_domains = vec![
                            self.private_access.profiles[profile_index]
                                .server
                                .trim()
                                .to_ascii_lowercase(),
                        ];
                        match self.merge_private_access_bypass_entries(&domains, &domain_suffixes) {
                            Ok(added) if added > 0 => self
                                .push_private_access_progress_for_profile(
                                    profile_index,
                                    PrivateAccessProgressTone::Success,
                                    format!("added {added} Private Access domain bypass rule(s)"),
                                ),
                            Ok(_) => {}
                            Err(error) => self.push_private_access_progress_for_profile(
                                profile_index,
                                PrivateAccessProgressTone::Error,
                                format!("failed to update system proxy bypass: {error:#}"),
                            ),
                        }
                        if matches!(
                            self.private_access.profiles[profile_index].mode,
                            PrivateAccessMode::Bridge
                        ) {
                            self.private_access.profiles[profile_index].bridge = bridge.clone();
                            let fallback_listen = self.private_access.profiles[profile_index]
                                .bridge_listen
                                .clone();
                            match self.apply_private_access_routes(
                                &profile_id,
                                &routes,
                                &domains,
                                &domain_suffixes,
                                &carrier_domains,
                                bridge,
                                &fallback_listen,
                            ) {
                                Ok(true) => {
                                    self.push_private_access_progress_for_profile(
                                        profile_index,
                                        PrivateAccessProgressTone::Success,
                                        "config.json 已更新".to_string(),
                                    );
                                    match self
                                        .restart_sing_box_for_private_access_progress(profile_index)
                                    {
                                        Ok(restart) => {
                                            self.set_status_only(format!(
                                                "Private Access {profile_id} ({service}) applied {} bridge route(s); {restart}",
                                                routes.len()
                                            ));
                                        }
                                        Err(error) => {
                                            let message = format!(
                                                "sing-box 重启失败，Private Access 不可用: {error:#}"
                                            );
                                            self.mark_private_access_integration_failed(
                                                profile_index,
                                                message.clone(),
                                            );
                                            self.set_status_only(message);
                                            stop_process = true;
                                        }
                                    }
                                }
                                Ok(false) => {
                                    self.push_private_access_progress_for_profile(
                                        profile_index,
                                        PrivateAccessProgressTone::Info,
                                        "没有需要写入的内网路由".to_string(),
                                    );
                                }
                                Err(error) => {
                                    self.private_access.profiles[profile_index].last_error =
                                        Some(error.to_string());
                                    self.private_access.profiles[profile_index].state =
                                        PrivateAccessState::Error;
                                    let message = format!("修改 config.json 失败: {error}");
                                    self.fail_private_access_progress_for_profile(
                                        profile_index,
                                        message.clone(),
                                    );
                                    self.set_status_only(message);
                                }
                            }
                        } else {
                            self.private_access.profiles[profile_index].bridge = None;
                            match self.apply_private_access_tun_routes(
                                &profile_id,
                                &routes,
                                &domains,
                                &domain_suffixes,
                                &carrier_domains,
                            ) {
                                Ok(true) => {
                                    self.push_private_access_progress_for_profile(
                                        profile_index,
                                        PrivateAccessProgressTone::Success,
                                        "config.json 已更新".to_string(),
                                    );
                                    if service == "sonicwall" {
                                        // SonicWall authentication and EVPN TLS use sing-box's
                                        // local HTTP CONNECT inbound. Restarting the managed core
                                        // here tears down the carrier beneath the new TUN.
                                        self.push_private_access_progress_for_profile(
                                            profile_index,
                                            PrivateAccessProgressTone::Info,
                                            "SonicWall 隧道已连接；为保持承载连接，本次不重启 sing-box"
                                                .to_string(),
                                        );
                                        self.finish_private_access_progress_for_profile(
                                            profile_index,
                                        );
                                        self.set_status_only(format!(
                                            "Private Access {profile_id} ({service}) connected; wrote {} TUN direct route(s) without restarting sing-box",
                                            routes.len()
                                        ));
                                    } else {
                                        match self.restart_sing_box_for_private_access_progress(
                                            profile_index,
                                        ) {
                                            Ok(restart) => {
                                                self.set_status_only(format!(
                                                    "Private Access {profile_id} ({service}) applied {} TUN direct route(s); {restart}",
                                                    routes.len()
                                                ));
                                            }
                                            Err(error) => {
                                                let message = format!(
                                                    "sing-box 重启失败，Private Access 不可用: {error:#}"
                                                );
                                                self.mark_private_access_integration_failed(
                                                    profile_index,
                                                    message.clone(),
                                                );
                                                self.set_status_only(message);
                                                stop_process = true;
                                            }
                                        }
                                    }
                                }
                                Ok(false) => {
                                    self.push_private_access_progress_for_profile(
                                        profile_index,
                                        PrivateAccessProgressTone::Info,
                                        "没有需要写入的内网路由".to_string(),
                                    );
                                }
                                Err(error) => {
                                    self.private_access.profiles[profile_index].last_error =
                                        Some(error.to_string());
                                    self.private_access.profiles[profile_index].state =
                                        PrivateAccessState::Error;
                                    let message = format!("修改 config.json 失败: {error}");
                                    self.fail_private_access_progress_for_profile(
                                        profile_index,
                                        message.clone(),
                                    );
                                    self.set_status_only(message);
                                }
                            }
                        }
                    }
                    PrivateAccessEvent::AuthChallenge {
                        service,
                        session_id,
                        challenge_id,
                        title,
                        message,
                        fields,
                        buttons,
                    } => {
                        self.private_access.profiles[profile_index].state =
                            PrivateAccessState::Connecting;
                        let profile = &self.private_access.profiles[profile_index];
                        let inputs = fields
                            .iter()
                            .map(|field| private_access_auth_initial_value(profile, field))
                            .collect();
                        self.private_access_auth = Some(PrivateAccessAuthModal {
                            profile_index,
                            service: service.clone(),
                            session_id,
                            challenge_id,
                            title: user_private_access_message(&title, "Private Access login"),
                            message,
                            fields,
                            buttons,
                            inputs,
                            field_index: 0,
                            error: None,
                        });
                        self.set_status_only(format!(
                            "Private Access {profile_id} ({service}) is waiting for authentication"
                        ));
                    }
                    PrivateAccessEvent::Error {
                        service,
                        code,
                        message,
                    } => {
                        let error = format!("{code}: {message}");
                        self.private_access.profiles[profile_index].last_error =
                            Some(error.clone());
                        self.private_access.profiles[profile_index].state =
                            PrivateAccessState::Error;
                        if self
                            .private_access_auth
                            .as_ref()
                            .is_some_and(|auth| auth.profile_index == profile_index)
                        {
                            self.private_access_auth = None;
                        }
                        self.fail_private_access_progress_for_profile(
                            profile_index,
                            format!("连接失败: {error}"),
                        );
                        if service == "sonicwall" {
                            self.push_private_access_progress_for_profile(
                                profile_index,
                                PrivateAccessProgressTone::Info,
                                "完整诊断已写入 sonicwall-private-access.log".to_string(),
                            );
                        } else if service == "hillstone" {
                            self.push_private_access_progress_for_profile(
                                profile_index,
                                PrivateAccessProgressTone::Info,
                                "完整诊断已写入 hillstone-private-access.log".to_string(),
                            );
                        }
                        self.set_status_only(format!(
                            "Private Access {profile_id} ({service}) error"
                        ));
                        stop_process = true;
                    }
                    PrivateAccessEvent::Log {
                        service: _,
                        message: _,
                    } => {
                        // Low-level service logs are intentionally not surfaced in the TUI flow.
                    }
                }
            }
            if stop_process
                && let Some(process) = self.private_access.profiles[profile_index].process.take()
            {
                process.stop()?;
            }
        }
        Ok(())
    }

    fn merge_private_access_bypass_entries(
        &mut self,
        domains: &[String],
        domain_suffixes: &[String],
    ) -> Result<usize> {
        let mut added = 0;
        for entry in domains.iter().chain(domain_suffixes) {
            for entry in parse_bypass_entries(entry) {
                if !self.bypass_entries.contains(&entry) {
                    self.bypass_entries.push(entry);
                    added += 1;
                }
            }
        }
        if added == 0 {
            return Ok(0);
        }

        self.save_runtime_state()?;
        self.save_bypass_rule_set()?;
        if self.system_proxy_enabled && self.system_proxy_job.is_none() {
            run_system_proxy_update(&self.system_proxy_server, true, &self.bypass_entries)
                .context("failed to refresh system proxy with Private Access bypass rules")?;
            self.system_proxy_enabled = system_proxy_matches(&self.system_proxy_server);
        }
        Ok(added)
    }

    fn apply_private_access_routes(
        &self,
        profile_id: &str,
        routes: &[PrivateAccessRoute],
        domains: &[String],
        domain_suffixes: &[String],
        carrier_domains: &[String],
        bridge: Option<PrivateAccessBridge>,
        fallback_listen: &str,
    ) -> Result<bool> {
        if routes.is_empty()
            && domains.is_empty()
            && domain_suffixes.is_empty()
            && carrier_domains.is_empty()
        {
            return Ok(false);
        }
        let listen = bridge
            .as_ref()
            .map(|bridge| bridge.listen.as_str())
            .unwrap_or(fallback_listen);
        let proxy = listen
            .parse::<SocketAddrV4>()
            .with_context(|| format!("private access bridge listen must be IPv4:PORT: {listen}"))?;
        // SET_ROUTE is pushed by the private-access gateway, so the TUI applies it as a profile
        // owned sing-box rule without a port matcher. That keeps one system proxy/TUN entry point
        // while still sending all matching intranet ports through the local bridge.
        let changed = run_private_access_route_table_config(
            &self.system_proxy_config_path,
            None,
            true,
            PrivateAccessRouteTableOptions {
                profile_id: profile_id.to_string(),
                cidrs: routes.iter().map(|route| route.cidr.clone()).collect(),
                domains: domains.to_vec(),
                domain_suffixes: domain_suffixes.to_vec(),
                carrier_domains: carrier_domains.to_vec(),
                proxy: Some(proxy),
            },
        )?;
        Ok(changed)
    }

    fn apply_private_access_tun_routes(
        &self,
        profile_id: &str,
        routes: &[PrivateAccessRoute],
        domains: &[String],
        domain_suffixes: &[String],
        carrier_domains: &[String],
    ) -> Result<bool> {
        if routes.is_empty()
            && domains.is_empty()
            && domain_suffixes.is_empty()
            && carrier_domains.is_empty()
        {
            return Ok(false);
        }
        // In TUN mode the kernel route points pushed intranet CIDRs at the helper-owned utun
        // interface. Browsers may still enter through sing-box's system proxy, so sing-box must
        // route those CIDRs to its direct outbound instead of the old local HTTP bridge override.
        let changed = run_private_access_route_table_config(
            &self.system_proxy_config_path,
            None,
            true,
            PrivateAccessRouteTableOptions {
                profile_id: profile_id.to_string(),
                cidrs: routes.iter().map(|route| route.cidr.clone()).collect(),
                domains: domains.to_vec(),
                domain_suffixes: domain_suffixes.to_vec(),
                carrier_domains: carrier_domains.to_vec(),
                proxy: None,
            },
        )?;
        Ok(changed)
    }

    fn mark_private_access_integration_failed(&mut self, profile_index: usize, message: String) {
        let profile = &mut self.private_access.profiles[profile_index];
        profile.integration_failed = true;
        profile.state = PrivateAccessState::Error;
        profile.last_error = Some(message.clone());
        self.fail_private_access_progress_for_profile(profile_index, message);
    }

    fn restart_sing_box_for_private_access_progress(
        &mut self,
        profile_index: usize,
    ) -> Result<String> {
        self.push_private_access_progress_for_profile(
            profile_index,
            PrivateAccessProgressTone::Info,
            "重启 sing-box 中...".to_string(),
        );
        match self.restart_managed_sing_box() {
            Ok(result) if result.restarted_pids.is_empty() => {
                let message = format!("sing-box 启动成功: pid {}", result.started_pid);
                self.push_private_access_progress_for_profile(
                    profile_index,
                    PrivateAccessProgressTone::Success,
                    message,
                );
                self.finish_private_access_progress_for_profile(profile_index);
                Ok(format!("started sing-box pid {}", result.started_pid))
            }
            Ok(result) => {
                let message = format!(
                    "sing-box 重启成功: pid(s) {:?} -> {}",
                    result.restarted_pids, result.started_pid
                );
                self.push_private_access_progress_for_profile(
                    profile_index,
                    PrivateAccessProgressTone::Success,
                    message,
                );
                self.finish_private_access_progress_for_profile(profile_index);
                Ok(format!(
                    "restarted sing-box pid(s) {:?} -> {}",
                    result.restarted_pids, result.started_pid
                ))
            }
            Err(error) => Err(error),
        }
    }

    fn set_system_proxy(&mut self) {
        if self.system_proxy_job.is_some() {
            self.set_status_only("System proxy update is already running");
            return;
        }
        if !self.system_proxy_server_override {
            self.system_proxy_server = default_system_proxy_server(&self.system_proxy_config_path);
        }
        self.system_proxy_enabled = system_proxy_matches(&self.system_proxy_server);
        let enable = !self.system_proxy_enabled;
        let server = self.system_proxy_server.clone();
        let bypass_entries = self.bypass_entries.clone();
        let (tx, rx) = mpsc::channel();
        let worker_server = server.clone();
        let worker = thread::spawn(move || {
            let result = run_system_proxy_update(&worker_server, enable, &bypass_entries)
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        self.system_proxy_job = Some(SystemProxyJob {
            server: server.clone(),
            enable,
            receiver: rx,
            worker,
        });
        if enable {
            self.set_status_only(format!("Enabling system proxy at {server}..."));
        } else {
            self.set_status_only("Disabling system proxy...");
        }
    }

    fn poll_system_proxy_updates(&mut self) {
        let Some(job) = self.system_proxy_job.as_ref() else {
            return;
        };
        let result = match job.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("system proxy worker disconnected".to_string()),
        };

        let job = self
            .system_proxy_job
            .take()
            .expect("system proxy job exists");
        let _ = job.worker.join();
        match result {
            Ok(message) => {
                self.system_proxy_server = job.server;
                self.system_proxy_enabled = if job.enable {
                    system_proxy_matches(&self.system_proxy_server)
                } else {
                    false
                };
                self.set_status_with_flash(message);
            }
            Err(error) => {
                self.system_proxy_enabled = system_proxy_matches(&self.system_proxy_server);
                self.set_status_with_flash(format!(
                    "System proxy update failed: {}",
                    truncate_for_width(&error, 90)
                ));
            }
        }
    }

    fn maybe_refresh_system_proxy_status(&mut self) {
        if self.system_proxy_job.is_some()
            || self.last_system_proxy_status_refresh.elapsed()
                < SYSTEM_PROXY_STATUS_REFRESH_INTERVAL
        {
            return;
        }
        self.last_system_proxy_status_refresh = Instant::now();
        if !self.system_proxy_server_override {
            self.system_proxy_server = default_system_proxy_server(&self.system_proxy_config_path);
        }
        self.system_proxy_enabled = system_proxy_matches(&self.system_proxy_server);
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

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.shutdown_managed_sing_box();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        App, AutoSelectSwitchPlan, BackgroundLatencyResult, BackgroundLatencySnapshot,
        CONNECTION_REFRESH_INTERVAL, DIRECT_CLASH_MODE, Focus, GLOBAL_CLASH_MODE,
        IntranetDetailSection, LATENCY_CHART_DEFAULT_WINDOW, LATENCY_CHART_REFRESH_INTERVAL,
        LatencyChartState, LatencyChartTimeUnit, LeftPaneSection, PrivateAccessMode,
        PrivateAccessProfileRuntime, PrivateAccessProgressTone, PrivateAccessRuntime,
        PrivateAccessState, RULE_CLASH_MODE, SYSTEM_PROXY_STATUS_REFRESH_INTERVAL,
        SettingsEditState, SettingsField, SingBoxProcessRuntime,
        command_matches_headless_auto_pick, command_matches_sing_box_run_for_config,
        config_arg_matches_path, connection_is_direct, format_bytes, format_connection_line,
        format_duration_badge, is_private_access_settings_field, latency_chart_segments,
        latency_chart_threshold_line, latency_chart_time_unit, latency_chart_windowed_samples,
        latency_chart_y_bounds, latency_chart_zoom_in, latency_chart_zoom_out, next_clash_mode,
        normalize_http_connect_proxy, private_access_auth_display_value,
        private_access_auth_initial_value, settings_field_display_value, settings_field_value,
        should_apply_private_access_state_after_integration, sing_box_config_args, status_lines,
        subscription_report_badge, system_proxy_bypass_entries, truncate_for_width,
        visible_settings_fields,
    };
    use crate::controller::{
        ApiClient, BenchmarkEvent, BenchmarkJob, BenchmarkJobKind, BenchmarkRequest,
        BenchmarkResult, BenchmarkSummary, ConnectionInfo, ConnectionMetadata, ConnectionsSnapshot,
        ProxyGroup,
    };
    use crate::defaults::{DEFAULT_BENCHMARK_MAX_CONCURRENCY, DEFAULT_CONTROLLER};
    use crate::private_access::{PrivateAccessAuthField, PrivateAccessBridge, PrivateAccessRoute};
    use crate::subscriptions::{ProviderRefreshSummary, SubscriptionRefreshOutput};
    use crate::tui_state::{PrivateAccessProfileState, TuiRuntimeState, TuiStateStore};
    use crossterm::event::KeyCode;
    use crossterm::event::MouseEventKind;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use reqwest::Client as AsyncClient;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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

    fn test_state_path() -> std::path::PathBuf {
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

    fn test_app() -> App {
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
            last_background_status_refresh: Instant::now()
                - super::BACKGROUND_STATUS_REFRESH_INTERVAL,
            last_background_status_generation: 0,
            background_started_at_unix: super::current_unix_timestamp(),
            background_worker: None,
            background_status_job: None,
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
            system_proxy_server: "127.0.0.1:6780".to_string(),
            system_proxy_server_override: false,
            system_proxy_enabled: false,
            system_proxy_job: None,
            last_system_proxy_status_refresh: Instant::now() - SYSTEM_PROXY_STATUS_REFRESH_INTERVAL,
            verify_job: None,
            sing_box: SingBoxProcessRuntime::new(false),
            private_access: PrivateAccessRuntime::with_default_hillstone()
                .expect("private access runtime"),
            private_access_progress: None,
            private_access_auth: None,
        }
    }

    fn line_text(line: ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn private_access_progress_text(app: &App) -> String {
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

    fn rendered_app_lines(app: &mut App) -> Vec<String> {
        let backend = TestBackend::new(140, 52);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| super::draw(frame, app))
            .expect("draw TUI");
        terminal
            .backend()
            .buffer()
            .content
            .chunks(140)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    fn rendered_app_text(app: &mut App) -> String {
        rendered_app_lines(app).join("\n")
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

    fn test_app_without_private_access() -> App {
        let mut app = test_app();
        app.private_access = PrivateAccessRuntime::new().expect("empty private access runtime");
        app
    }

    #[test]
    fn left_pane_hides_intranet_section_without_configured_profiles() {
        let mut app = test_app_without_private_access();

        let screen = rendered_app_text(&mut app);

        assert!(screen.contains("Internet Proxy"));
        assert!(!screen.contains("Intranet Proxy"));
    }

    #[test]
    fn selected_private_access_profile_renders_intranet_details() {
        let mut app = test_app();
        let profile = app.private_access.focused_mut();
        profile.server = "vpn.example.com".to_string();
        profile.state = PrivateAccessState::Connected;
        profile.routes = vec![
            PrivateAccessRoute {
                cidr: "10.20.0.0/16".to_string(),
            },
            PrivateAccessRoute {
                cidr: "172.20.4.0/24".to_string(),
            },
        ];
        profile.dns = vec!["10.20.0.53".to_string()];
        profile.domains = vec!["portal.internal.example".to_string()];
        profile.domain_suffixes = vec!["corp.example".to_string()];
        app.focus = Focus::Groups;
        app.left_pane_section = LeftPaneSection::Intranet;

        let screen = rendered_app_text(&mut app);

        assert!(screen.contains("Internet Proxy"));
        assert!(screen.contains("Intranet Proxy"));
        assert!(screen.contains("Intranet: hillstone"));
        assert!(screen.contains("vpn.example.com:4433"));
        assert!(screen.contains("10.20.0.0/16"));
        assert!(screen.contains("10.20.0.53"));
        assert!(screen.contains("portal.internal.example"));
        assert!(screen.contains("*.corp.example"));

        app.focus = Focus::Members;
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

        let collapsed = app.intranet_detail_view(app.private_access.focused());
        let route_range = collapsed
            .sections
            .iter()
            .find(|range| range.section == IntranetDetailSection::Routes)
            .copied()
            .expect("routes section");
        assert!(route_range.foldable);
        let collapsed_text = collapsed
            .lines
            .iter()
            .cloned()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(collapsed_text.contains("▶ Routes (103)"));
        assert!(collapsed_text.contains("… 93 more item(s)"));
        assert!(collapsed_text.contains("10.20.9.0/24"));
        assert!(!collapsed_text.contains("10.20.10.0/24"));

        app.intranet_detail_scroll = route_range.start as u16;
        app.handle_key(KeyCode::Enter).expect("expand routes");
        let expanded = app.intranet_detail_view(app.private_access.focused());
        assert_eq!(expanded.lines.len(), collapsed.lines.len() + 92);
        assert!(
            expanded
                .lines
                .iter()
                .cloned()
                .map(line_text)
                .any(|line| line.contains("▼ Routes (103)"))
        );

        app.handle_key(KeyCode::Enter).expect("fold routes");
        let folded_again = app.intranet_detail_view(app.private_access.focused());
        assert_eq!(folded_again.lines.len(), collapsed.lines.len());
    }

    #[test]
    fn intranet_footer_stays_fixed_and_status_omits_private_access_summary() {
        let mut app = test_app();
        app.private_access.focused_mut().routes = (0..40)
            .map(|index| PrivateAccessRoute {
                cidr: format!("172.20.{index}.0/24"),
            })
            .collect();
        app.focus = Focus::Members;
        app.left_pane_section = LeftPaneSection::Intranet;

        let first = rendered_app_lines(&mut app);
        let footer_row = first
            .iter()
            .position(|line| line.contains("Enter expand/fold"))
            .expect("fixed intranet footer");
        app.intranet_detail_scroll = 8;
        let scrolled = rendered_app_lines(&mut app);
        assert_eq!(
            scrolled
                .iter()
                .position(|line| line.contains("Enter expand/fold")),
            Some(footer_row)
        );
        assert!(
            status_lines(&app)
                .into_iter()
                .map(line_text)
                .all(|line| !line.starts_with("private access:"))
        );
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
    fn detects_mixed_inbound_proxy_server_from_config() {
        let path = std::env::temp_dir().join("sing-box-tui-proxy-config-test.json");
        std::fs::write(
            &path,
            r#"{"inbounds":[{"type":"mixed","listen":"::","listen_port":6780}]}"#,
        )
        .expect("write config");

        assert_eq!(
            super::detect_mixed_inbound_proxy_server(&path).as_deref(),
            Some("127.0.0.1:6780")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn detects_mixed_inbound_proxy_server_from_partial_text_when_config_is_invalid() {
        let text = r#"{
            "inbounds": [
                {"type":"mixed","listen":"::","listen_port":6780}
            ],
            "outbounds": [broken
        }"#;

        assert_eq!(
            super::detect_mixed_inbound_proxy_server_from_text(text).as_deref(),
            Some("127.0.0.1:6780")
        );
    }

    #[test]
    fn system_proxy_bypass_entries_include_tui_bypass_rules() {
        let entries = system_proxy_bypass_entries(&[
            "example.com".to_string(),
            "*.github.com".to_string(),
            "10.0.0.0/8".to_string(),
            "1.1.1.1".to_string(),
        ]);

        assert!(entries.contains(&"localhost".to_string()));
        assert!(entries.contains(&"example.com".to_string()));
        assert!(entries.contains(&"*.example.com".to_string()));
        assert!(entries.contains(&"*.github.com".to_string()));
        assert!(entries.contains(&"10.0.0.0/8".to_string()));
        assert!(entries.contains(&"1.1.1.1".to_string()));
    }

    #[test]
    fn private_access_domains_are_persisted_as_system_proxy_bypass_entries() {
        let mut app = test_app();
        let added = app
            .merge_private_access_bypass_entries(
                &["service.hundsun.com".to_string()],
                &["Hundsun.COM".to_string(), "hs.handsome.com.cn".to_string()],
            )
            .expect("private access bypass entries merge");

        assert_eq!(added, 3);
        assert_eq!(
            app.bypass_entries,
            vec![
                "service.hundsun.com".to_string(),
                "hundsun.com".to_string(),
                "hs.handsome.com.cn".to_string(),
            ]
        );
        assert_eq!(
            app.merge_private_access_bypass_entries(
                &["SERVICE.HUNDSUN.COM".to_string()],
                &["*.hundsun.com".to_string()],
            )
            .expect("duplicate private access bypass entries merge"),
            0
        );
    }

    #[test]
    fn system_proxy_bypass_entries_do_not_bypass_private_ranges_by_default() {
        let entries = system_proxy_bypass_entries(&[]);

        assert!(entries.contains(&"localhost".to_string()));
        assert!(entries.contains(&"127.*".to_string()));
        assert!(!entries.contains(&"10.*".to_string()));
        assert!(!entries.contains(&"172.16.*".to_string()));
        assert!(!entries.contains(&"192.168.*".to_string()));
    }

    #[test]
    fn system_proxy_bypass_entries_include_cgnat_overlay_range() {
        let entries = system_proxy_bypass_entries(&[]);

        assert!(entries.contains(&"100.64.*".to_string()));
        assert!(entries.contains(&"100.121.*".to_string()));
        assert!(entries.contains(&"100.127.*".to_string()));
        assert!(!entries.contains(&"100.128.*".to_string()));
    }

    #[test]
    fn sing_box_process_matcher_accepts_run_command_for_config() {
        let config = PathBuf::from("config.json");

        assert!(command_matches_sing_box_run_for_config(
            "sing-box run --config ./config.json",
            &config
        ));
        assert!(command_matches_sing_box_run_for_config(
            "/usr/local/bin/sing-box run -c config.json",
            &config
        ));
        assert!(command_matches_sing_box_run_for_config(
            "sing-box run --config=/Users/ldd/proj/rust/sing-box-tui/config.json",
            &config
        ));
    }

    #[test]
    fn sing_box_process_matcher_rejects_non_matching_commands() {
        let config = PathBuf::from("config.json");

        assert!(!command_matches_sing_box_run_for_config(
            "sing-box version",
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            "target/debug/sing-box-tui",
            &config
        ));
        assert!(!command_matches_sing_box_run_for_config(
            "sing-box run --config ./other.json",
            &config
        ));
    }

    #[test]
    fn headless_auto_pick_process_matcher_accepts_worker_command() {
        assert!(command_matches_headless_auto_pick(
            "/Users/ldd/proj/rust/sing-box-tui/target/debug/sing-box-tui run --headless-auto-pick --controller http://127.0.0.1:9992"
        ));
        assert!(command_matches_headless_auto_pick(
            "target/debug/sing-box-tui run --controller http://127.0.0.1:9992 --headless-auto-pick"
        ));
        assert!(command_matches_headless_auto_pick(
            r#""C:\Program Files\sing-box-tui\sing-box-tui.exe" run --headless-auto-pick --controller http://127.0.0.1:9992"#
        ));
        assert!(command_matches_headless_auto_pick(
            r#"C:\tools\sing-box-tui.exe run --controller http://127.0.0.1:9992 --headless-auto-pick"#
        ));
    }

    #[test]
    fn headless_auto_pick_process_matcher_rejects_non_workers() {
        assert!(!command_matches_headless_auto_pick(
            "target/debug/sing-box-tui"
        ));
        assert!(!command_matches_headless_auto_pick(
            "target/debug/sing-box-tui run --controller http://127.0.0.1:9992"
        ));
        assert!(!command_matches_headless_auto_pick(
            "sing-box run --headless-auto-pick"
        ));
    }

    #[test]
    fn background_bind_rejects_remote_addresses_without_explicit_allow() {
        assert!(super::validate_background_bind_addr_with_remote("127.0.0.1:0", false).is_ok());
        assert!(super::validate_background_bind_addr_with_remote("[::1]:0", false).is_ok());

        let error = super::validate_background_bind_addr_with_remote("0.0.0.0:9999", false)
            .expect_err("remote bind requires explicit allow");
        assert!(format!("{error:#}").contains("refusing non-loopback"));
        assert!(super::validate_background_bind_addr_with_remote("0.0.0.0:9999", true).is_ok());
    }

    #[test]
    fn read_text_tail_handles_missing_empty_small_and_large_files() {
        let missing = std::env::temp_dir().join(format!(
            "sing-box-tui-missing-log-{}.log",
            unique_test_suffix()
        ));
        assert_eq!(super::read_text_tail(&missing, 16), None);

        let path = std::env::temp_dir().join(format!(
            "sing-box-tui-tail-log-{}.log",
            unique_test_suffix()
        ));
        std::fs::write(&path, "").expect("empty log writes");
        assert_eq!(super::read_text_tail(&path, 16), None);

        std::fs::write(&path, "first\nsecond\n").expect("small log writes");
        assert_eq!(
            super::read_text_tail(&path, 1024),
            Some("first\nsecond".to_string())
        );

        std::fs::write(&path, "0123456789abcdef").expect("large log writes");
        assert_eq!(super::read_text_tail(&path, 6), Some("abcdef".to_string()));

        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn background_task_state_is_written_with_private_permissions() {
        let path = std::env::temp_dir().join(format!(
            "sing-box-tui-background-test-{}.json",
            unique_test_suffix()
        ));
        let state = super::BackgroundTaskState {
            version: 2,
            kind: super::BACKGROUND_TASK_KIND_AUTO_PICK.to_string(),
            pid: 42,
            controller: DEFAULT_CONTROLLER.to_string(),
            config_path: PathBuf::from("config.json"),
            max_concurrency: 16,
            started_at_unix: 1,
            status_generation: 0,
            status: Some("starting".to_string()),
            updated_at_unix: Some(1),
            bind_addr: "127.0.0.1:9999".to_string(),
            token: "secret".to_string(),
        };

        super::write_background_task_state_to_path(&path, &state).expect("state writes");

        let mode = std::fs::metadata(&path)
            .expect("state metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn background_task_state_refuses_symlink_path() {
        let target = std::env::temp_dir().join(format!(
            "sing-box-tui-background-target-{}.json",
            unique_test_suffix()
        ));
        let link = std::env::temp_dir().join(format!(
            "sing-box-tui-background-link-{}.json",
            unique_test_suffix()
        ));
        std::fs::write(&target, "{}").expect("target writes");
        std::os::unix::fs::symlink(&target, &link).expect("symlink writes");

        let state = super::BackgroundTaskState {
            version: 2,
            kind: super::BACKGROUND_TASK_KIND_AUTO_PICK.to_string(),
            pid: 42,
            controller: DEFAULT_CONTROLLER.to_string(),
            config_path: PathBuf::from("config.json"),
            max_concurrency: 16,
            started_at_unix: 1,
            status_generation: 0,
            status: Some("starting".to_string()),
            updated_at_unix: Some(1),
            bind_addr: "127.0.0.1:9999".to_string(),
            token: "secret".to_string(),
        };

        let error = super::write_background_task_state_to_path(&link, &state)
            .expect_err("symlink is rejected");
        assert!(format!("{error:#}").contains("refusing to write"));

        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn sing_box_config_args_support_common_forms() {
        assert_eq!(
            sing_box_config_args(&["sing-box", "run", "--config", "./config.json"]),
            vec!["./config.json"]
        );
        assert_eq!(
            sing_box_config_args(&["sing-box", "run", "-c=config.json"]),
            vec!["config.json"]
        );
        assert!(config_arg_matches_path(
            "./config.json",
            &PathBuf::from("config.json")
        ));
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
            "connections active=2 proxy=1 direct=1 up=1.5KiB down=2.0MiB  c details"
        );
    }

    #[test]
    fn connection_helpers_format_active_rows() {
        let direct = test_connection("example.cn", vec!["国内直连"]);
        let proxied = test_connection("www.google.com", vec!["node-a", "airtcp", "手动选择"]);

        assert!(connection_is_direct(&direct));
        assert!(!connection_is_direct(&proxied));
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0KiB");
        assert!(format_connection_line(&proxied, 120).contains("www.google.com:443"));
        assert!(format_connection_line(&proxied, 120).contains("node-a -> airtcp"));
    }

    #[test]
    fn subscription_report_badge_summarizes_provider_counts() {
        let report = SubscriptionRefreshOutput {
            input_path: ".suburl".to_string(),
            cache_path: ".suburl.cache.json".to_string(),
            interval_days: 1,
            merged_config_path: "/usr/local/etc/sing-box/config.json".to_string(),
            backup_config_path: Some(
                "/usr/local/etc/sing-box/config.json.sing-box-tui-subscription-backup".to_string(),
            ),
            providers: vec![
                ProviderRefreshSummary {
                    provider: "宝贝云".to_string(),
                    subscription_url: "https://example.com?token=REDACTED".to_string(),
                    status: "fetched".to_string(),
                    imported_nodes: 67,
                    fetched_at_unix: 10,
                    warning: None,
                },
                ProviderRefreshSummary {
                    provider: "airtcp".to_string(),
                    subscription_url: "https://example.com/link/REDACTED".to_string(),
                    status: "cached".to_string(),
                    imported_nodes: 0,
                    fetched_at_unix: 10,
                    warning: Some("no mergeable nodes found".to_string()),
                },
            ],
        };

        let badge = subscription_report_badge(&report);

        assert!(badge.contains("宝贝云:fetched:67 nodes"));
        assert!(badge.contains("airtcp:cached:0 nodes"));
        assert!(badge.contains("no mergeable nodes found"));
    }

    #[test]
    fn duration_badge_uses_day_hour_minute_units() {
        assert_eq!(format_duration_badge(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration_badge(Duration::from_secs(5 * 60)), "5m");
        assert_eq!(
            format_duration_badge(Duration::from_secs(2 * 3600 + 30 * 60)),
            "2h30m"
        );
        assert_eq!(
            format_duration_badge(Duration::from_secs(24 * 3600 + 3600)),
            "1d1h"
        );
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
        assert!(app.private_access.summary_line().is_none());
        assert!(app.runtime_state().private_access_profiles.is_empty());
        assert!(
            visible_settings_fields(&app)
                .iter()
                .all(|field| !is_private_access_settings_field(*field))
        );
    }

    #[test]
    fn pressing_uppercase_v_without_private_access_profiles_does_not_load_default_profile() {
        let mut app = test_app_without_private_access();

        app.handle_key(KeyCode::Char('V'))
            .expect("missing private access profiles is handled");

        assert!(!app.private_access.is_configured());
        assert!(app.private_access_progress.is_none());
        assert_eq!(app.status, "Private Access is not configured");
        assert!(app.runtime_state().private_access_profiles.is_empty());
    }

    #[test]
    fn pressing_uppercase_v_without_private_access_settings_opens_progress_modal() {
        let mut app = test_app();
        app.private_access.focused_mut().server.clear();
        app.private_access.focused_mut().username.clear();

        app.handle_key(KeyCode::Char('V'))
            .expect("private access missing settings is handled");

        assert!(
            private_access_progress_text(&app)
                .contains("请先在 settings 中配置 Private Access server")
        );
        assert!(
            app.private_access_progress
                .as_ref()
                .is_some_and(|progress| progress.done)
        );
        assert!(app.private_access.focused().process.is_none());
    }

    #[test]
    fn private_access_progress_title_follows_event_profile_not_focus() {
        let mut app = test_app();
        app.private_access
            .profiles
            .push(PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile"));
        app.private_access.focused_index = 1;
        app.open_private_access_progress();
        assert_eq!(
            app.private_access_progress
                .as_ref()
                .expect("focused progress")
                .title,
            "Private Access - sonicwall (tun)"
        );

        app.push_private_access_progress_for_profile(
            0,
            PrivateAccessProgressTone::Error,
            "连接失败: session_failed".to_string(),
        );

        let progress = app
            .private_access_progress
            .as_ref()
            .expect("event profile progress");
        assert_eq!(progress.profile_index, 0);
        assert_eq!(progress.title, "Private Access - hillstone (bridge)");
        assert!(private_access_progress_text(&app).contains("session_failed"));
    }

    #[test]
    fn private_access_service_spawn_failure_stays_inside_tui_state() {
        let mut app = test_app();
        app.private_access.focused_mut().server = "sslvpn.example.com".to_string();
        app.private_access.focused_mut().username = "alice".to_string();
        app.private_access.focused_mut().manifest.executable =
            "/path/that/does/not/exist/private-access-service".to_string();

        app.handle_key(KeyCode::Char('V'))
            .expect("private access spawn failure is handled");

        assert_eq!(
            app.private_access.focused().state,
            PrivateAccessState::Error
        );
        assert!(app.private_access.focused().process.is_none());
        assert!(private_access_progress_text(&app).contains("启动 Private Access service 失败"));
        assert!(
            app.private_access_progress
                .as_ref()
                .is_some_and(|progress| progress.done)
        );
    }

    #[test]
    fn official_sonicwall_process_warning_names_conflicting_clients() {
        let warning = super::format_official_sonicwall_client_warning(&[
            "SnwlVpn.exe".to_string(),
            "SnwlConnect.exe".to_string(),
        ]);
        assert!(warning.contains("SnwlVpn.exe"));
        assert!(warning.contains("SnwlConnect.exe"));
        assert!(warning.contains("SonicWall"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_tasklist_parser_extracts_image_names() {
        let names = super::parse_windows_tasklist_image_names(
            "\"SnwlVpn.exe\",\"5596\",\"Console\",\"1\",\"12,344 K\"\r\n\
             \"SnwlConnect.exe\",\"8604\",\"Console\",\"1\",\"80,120 K\"\r\n",
        );
        assert_eq!(names, vec!["SnwlVpn.exe", "SnwlConnect.exe"]);
    }

    #[test]
    fn private_access_integration_failure_blocks_late_success_state() {
        let mut app = test_app();
        let profile = app.private_access.focused_mut();
        profile.integration_failed = true;
        profile.state = PrivateAccessState::Error;
        profile.last_error = Some("sing-box restart failed".to_string());

        assert!(!should_apply_private_access_state_after_integration(
            app.private_access.focused(),
            &PrivateAccessState::Connected
        ));
        assert!(!should_apply_private_access_state_after_integration(
            app.private_access.focused(),
            &PrivateAccessState::Connecting
        ));
        assert!(should_apply_private_access_state_after_integration(
            app.private_access.focused(),
            &PrivateAccessState::Error
        ));
        assert!(should_apply_private_access_state_after_integration(
            app.private_access.focused(),
            &PrivateAccessState::Disconnected
        ));
    }

    #[test]
    fn sonicwall_gateway_proxy_exception_is_written_before_sing_box_startup() {
        let mut app = test_app();
        app.private_access = PrivateAccessRuntime {
            profiles: vec![
                PrivateAccessProfileRuntime::default_sonicwall().expect("SonicWall profile"),
            ],
            focused_index: 0,
        };
        let config_path = test_state_path();
        let config = json!({
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "selector", "tag": "select", "outbounds": ["direct"] }
            ],
            "route": {
                "rules": [
                    { "action": "hijack-dns", "protocol": "dns" },
                    {
                        "action": "route",
                        "rule_set": ["sing-box-tui-bypass"],
                        "outbound": "direct"
                    },
                    {
                        "action": "route",
                        "domain_suffix": ["hundsun.com"],
                        "outbound": "direct"
                    }
                ]
            }
        });
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("config writes");
        app.system_proxy_config_path = config_path.clone();

        let changed = app
            .ensure_private_access_carrier_routes()
            .expect("carrier route is written");

        assert!(changed);
        let text = std::fs::read_to_string(&config_path).expect("config reads");
        let config: serde_json::Value = serde_json::from_str(&text).expect("config parses");
        let rules = config["route"]["rules"].as_array().expect("route rules");
        let carrier_index = rules
            .iter()
            .position(|rule| rule["domain"] == json!(["sslvpn.hundsun.com"]))
            .expect("SonicWall carrier rule exists");
        let bypass_index = rules
            .iter()
            .position(|rule| rule["rule_set"] == json!(["sing-box-tui-bypass"]))
            .expect("generic bypass rule exists");
        let internal_index = rules
            .iter()
            .position(|rule| rule["domain_suffix"] == json!(["hundsun.com"]))
            .expect("internal domain rule exists");
        assert!(carrier_index < bypass_index);
        assert!(carrier_index < internal_index);
        assert_eq!(rules[carrier_index]["outbound"], "select");
        let _ = std::fs::remove_file(config_path);
    }

    #[test]
    fn private_access_route_application_writes_pushed_cidrs_without_port_matcher() {
        let mut app = test_app();
        let config_path = test_state_path();
        let config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": {
                "rules": [
                    { "ip_is_private": true, "outbound": "direct" }
                ]
            }
        });
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("config writes");
        app.system_proxy_config_path = config_path.clone();

        let applied = app
            .apply_private_access_routes(
                "hillstone",
                &[PrivateAccessRoute {
                    cidr: "10.1.0.0/16".to_string(),
                }],
                &[],
                &[],
                &[],
                Some(PrivateAccessBridge {
                    kind: "http".to_string(),
                    listen: "127.0.0.1:16780".to_string(),
                }),
                "127.0.0.1:16780",
            )
            .expect("private access routes apply");

        assert!(applied);
        let text = std::fs::read_to_string(&config_path).expect("config reads");
        let config: serde_json::Value = serde_json::from_str(&text).expect("config parses");
        let rules = config["route"]["rules"].as_array().expect("route rules");
        let rule = rules
            .iter()
            .find(|rule| rule["ip_cidr"] == json!(["10.1.0.0/16"]))
            .expect("private access rule exists");
        assert!(rule.get("port").is_none());
        assert_eq!(rule["override_address"], "127.0.0.1");
        assert_eq!(rule["override_port"], 16780);
        let _ = std::fs::remove_file(config_path);
    }

    #[test]
    fn private_access_route_application_keeps_profile_bridges_separate() {
        let mut app = test_app();
        let config_path = test_state_path();
        let config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "rules": [] }
        });
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("config writes");
        app.system_proxy_config_path = config_path.clone();

        app.apply_private_access_routes(
            "office",
            &[PrivateAccessRoute {
                cidr: "10.1.0.0/16".to_string(),
            }],
            &[],
            &[],
            &[],
            None,
            "127.0.0.1:16780",
        )
        .expect("office routes apply");
        app.apply_private_access_routes(
            "lab",
            &[PrivateAccessRoute {
                cidr: "10.2.0.0/16".to_string(),
            }],
            &[],
            &[],
            &[],
            None,
            "127.0.0.1:18081",
        )
        .expect("lab routes apply");

        let text = std::fs::read_to_string(&config_path).expect("config reads");
        let config: serde_json::Value = serde_json::from_str(&text).expect("config parses");
        let rules = config["route"]["rules"].as_array().expect("route rules");
        let office = rules
            .iter()
            .find(|rule| rule["ip_cidr"] == json!(["10.1.0.0/16"]))
            .expect("office route exists");
        let lab = rules
            .iter()
            .find(|rule| rule["ip_cidr"] == json!(["10.2.0.0/16"]))
            .expect("lab route exists");
        assert_eq!(office["override_port"], 16780);
        assert_eq!(lab["override_port"], 18081);
        let _ = std::fs::remove_file(config_path);
    }

    #[test]
    fn private_access_tun_route_application_removes_bridge_override() {
        let app = test_app();
        let config_path = test_state_path();
        let config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": {
                "rules": [{
                    "action": "route",
                    "ip_cidr": ["10.1.0.0/16", "10.255.0.0/24"],
                    "outbound": "direct",
                    "override_address": "127.0.0.1",
                    "override_port": 18080
                }]
            }
        });
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("config writes");
        let mut app = app;
        app.system_proxy_config_path = config_path.clone();

        app.apply_private_access_tun_routes(
            "hillstone",
            &[
                PrivateAccessRoute {
                    cidr: "10.1.0.0/16".to_string(),
                },
                PrivateAccessRoute {
                    cidr: "10.255.0.0/24".to_string(),
                },
                PrivateAccessRoute {
                    cidr: "10.253.0.0/24".to_string(),
                },
            ],
            &[],
            &[],
            &[],
        )
        .expect("TUN routes apply");

        let text = std::fs::read_to_string(&config_path).expect("config reads");
        let config: serde_json::Value = serde_json::from_str(&text).expect("config parses");
        assert_eq!(config["route"]["auto_detect_interface"], false);
        let rules = config["route"]["rules"].as_array().expect("route rules");
        let rule = rules
            .iter()
            .find(|rule| {
                rule["ip_cidr"] == json!(["10.1.0.0/16", "10.255.0.0/24", "10.253.0.0/24"])
            })
            .expect("private access TUN direct rule exists");
        assert_eq!(rule["outbound"], "direct");
        assert!(rule.get("override_address").is_none());
        assert!(rule.get("override_port").is_none());
        let _ = std::fs::remove_file(config_path);
    }

    #[test]
    fn private_access_summary_shows_explicit_state_and_details() {
        let mut app = test_app();
        let focused = app.private_access.focused_mut();
        focused.state = PrivateAccessState::Connected;
        focused.routes = vec![PrivateAccessRoute {
            cidr: "10.1.0.0/16".to_string(),
        }];
        focused.bridge = Some(PrivateAccessBridge {
            kind: "http".to_string(),
            listen: "127.0.0.1:16780".to_string(),
        });

        let connected = line_text(app.private_access.summary_line().expect("summary line"));
        assert!(connected.contains("[>hillstone CONNECTED]"));
        assert!(connected.contains("routes=1"));
        assert!(connected.contains("mode=bridge"));
        assert!(connected.contains("bridge=127.0.0.1:16780"));

        let focused = app.private_access.focused_mut();
        focused.state = PrivateAccessState::Error;
        focused.last_error = Some("session_failed: auth rejected".to_string());

        let errored = line_text(app.private_access.summary_line().expect("summary line"));
        assert!(errored.contains("[>hillstone ERROR]"));
        assert!(errored.contains("error=session_failed: auth rejected"));
    }

    #[test]
    fn private_access_tun_summary_does_not_show_bridge_listener() {
        let mut app = test_app();
        let focused = app.private_access.focused_mut();
        focused.mode = PrivateAccessMode::Tun;
        focused.state = PrivateAccessState::Connected;
        focused.routes = vec![PrivateAccessRoute {
            cidr: "10.1.0.0/16".to_string(),
        }];

        let summary = line_text(app.private_access.summary_line().expect("summary line"));

        assert!(summary.contains("mode=tun"));
        assert!(summary.contains("data=tun"));
        assert!(!summary.contains("bridge=127.0.0.1:16780"));
    }

    #[test]
    fn private_access_background_session_from_state_is_shown_as_background() {
        let mut app = test_app();
        let pid = std::process::id();
        let state = TuiRuntimeState {
            private_access_profiles: vec![PrivateAccessProfileState {
                id: "hillstone".to_string(),
                server: Some("sslvpn.example.com".to_string()),
                username: Some("alice".to_string()),
                background_pid: Some(pid),
                ..PrivateAccessProfileState::default()
            }],
            ..TuiRuntimeState::default()
        };

        app.apply_runtime_state(state).expect("state applies");

        assert_eq!(
            app.private_access.focused().state,
            PrivateAccessState::Connected
        );
        assert_eq!(app.private_access.focused().background_pid, Some(pid));
        let summary = line_text(app.private_access.summary_line().expect("summary line"));
        assert!(summary.contains("BACKGROUND"), "{summary}");
        assert!(summary.contains(&format!("pid={pid}")), "{summary}");
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
        let summary = line_text(app.private_access.summary_line().expect("summary line"));
        assert!(summary.contains("mode=tun"));
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
        assert_eq!(app.private_access.focused().mode, PrivateAccessMode::Bridge);
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
    fn interactive_sudo_tun_helper_needs_terminal_prompt() {
        let mut app = test_app();
        app.private_access.focused_mut().mode = PrivateAccessMode::Tun;
        app.private_access.focused_mut().tun_helper = vec![
            "sudo".to_string(),
            "target/debug/sing-box-tui".to_string(),
            "private-access-tun-helper".to_string(),
            "--stdio".to_string(),
        ];

        assert!(app.private_access_connect_needs_terminal_prompt());

        app.private_access.focused_mut().tun_helper = vec![
            "sudo".to_string(),
            "-n".to_string(),
            "target/debug/sing-box-tui".to_string(),
            "private-access-tun-helper".to_string(),
            "--stdio".to_string(),
        ];

        assert!(!app.private_access_connect_needs_terminal_prompt());
    }

    #[test]
    fn tun_mode_without_persisted_helper_uses_interactive_tui_helper() {
        let mut app = test_app();
        app.private_access.focused_mut().mode = PrivateAccessMode::Tun;
        app.private_access.focused_mut().tun_helper.clear();

        let command = app
            .private_access_tun_helper_for_connect(app.private_access.focused())
            .expect("tun helper command");
        #[cfg(unix)]
        {
            assert!(app.private_access_connect_needs_terminal_prompt());
            assert_eq!(command.first().map(String::as_str), Some("sudo"));
            assert!(!command.iter().any(|arg| arg == "-n"));
        }
        #[cfg(not(unix))]
        {
            assert!(!app.private_access_connect_needs_terminal_prompt());
            assert_ne!(command.first().map(String::as_str), Some("sudo"));
        }
        assert!(command.iter().any(|arg| arg == "private-access-tun-helper"));
        assert!(command.iter().any(|arg| arg == "--stdio"));
        assert!(
            app.runtime_state().private_access_profiles[0]
                .tun_helper
                .is_none()
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
        app.sing_box.keep_running = false;

        let keep_running = app.handle_key(KeyCode::Char('B')).expect("handle key");

        assert!(!keep_running);
        assert!(app.sing_box.keep_running);
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
    fn latency_chart_segments_break_on_failed_samples() {
        let samples = vec![
            NodeLatencySample {
                recorded_at_ms: 1_000,
                delay_ms: Some(90),
            },
            NodeLatencySample {
                recorded_at_ms: 2_000,
                delay_ms: None,
            },
            NodeLatencySample {
                recorded_at_ms: 3_000,
                delay_ms: Some(120),
            },
        ];

        assert_eq!(
            latency_chart_segments(&samples),
            vec![vec![(1_000, 90)], vec![(3_000, 120)]]
        );
    }

    #[test]
    fn latency_chart_uses_minutes_for_short_windows_and_hours_for_long_windows() {
        assert_eq!(
            latency_chart_time_unit(Duration::from_secs(30 * 60)),
            LatencyChartTimeUnit::Minutes
        );
        assert_eq!(
            latency_chart_time_unit(Duration::from_secs(3 * 60 * 60)),
            LatencyChartTimeUnit::Hours
        );
    }

    #[test]
    fn latency_chart_zoom_adjusts_window() {
        assert_eq!(
            latency_chart_zoom_in(Duration::from_secs(60 * 60)),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            latency_chart_zoom_out(Duration::from_secs(60 * 60)),
            Duration::from_secs(2 * 60 * 60)
        );
    }

    #[test]
    fn latency_chart_threshold_line_spans_the_visible_window() {
        assert_eq!(
            latency_chart_threshold_line(30.0, 600),
            vec![(0.0, 600.0), (30.0, 600.0)]
        );
    }

    #[test]
    fn latency_chart_y_bounds_include_threshold() {
        let low_latency_bounds = latency_chart_y_bounds(80.0, 120.0, 600);
        assert!(low_latency_bounds[0] <= 80.0);
        assert!(low_latency_bounds[1] > 600.0);

        let high_latency_bounds = latency_chart_y_bounds(700.0, 900.0, 600);
        assert!(high_latency_bounds[0] < 600.0);
        assert!(high_latency_bounds[1] >= 900.0);
    }

    #[test]
    fn latency_chart_window_keeps_recent_samples() {
        let samples = vec![
            NodeLatencySample {
                recorded_at_ms: 0,
                delay_ms: Some(90),
            },
            NodeLatencySample {
                recorded_at_ms: 45 * 60 * 1000,
                delay_ms: Some(120),
            },
            NodeLatencySample {
                recorded_at_ms: 60 * 60 * 1000,
                delay_ms: Some(80),
            },
        ];

        let visible = latency_chart_windowed_samples(&samples, Duration::from_secs(30 * 60));

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].recorded_at_ms, 45 * 60 * 1000);
        assert_eq!(visible[1].recorded_at_ms, 60 * 60 * 1000);
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
    fn live_background_worker_poll_failure_retries_without_reconnect() {
        let retry = super::resolve_background_status_poll(super::BackgroundStatusPollOutcome {
            result: Err("temporary TCP timeout".to_string()),
            process_alive: true,
        });
        assert!(matches!(
            retry,
            super::BackgroundStatusPollResolution::Retry(error)
                if error == "temporary TCP timeout"
        ));

        let reconnect = super::resolve_background_status_poll(super::BackgroundStatusPollOutcome {
            result: Err("worker exited".to_string()),
            process_alive: false,
        });
        assert!(matches!(
            reconnect,
            super::BackgroundStatusPollResolution::Reconnect(error)
                if error == "worker exited"
        ));
    }

    #[test]
    fn process_exists_recognizes_current_process() {
        assert!(super::process_exists(std::process::id()));
    }

    #[test]
    fn background_tcp_control_round_trips_status_with_token() {
        let (addr, rx) = super::spawn_background_tcp_server("127.0.0.1:0", "secret".to_string())
            .expect("tcp server starts");
        let worker = thread::spawn(move || {
            let request = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request received");
            assert!(matches!(
                request.command,
                super::BackgroundWorkerCommand::Status
            ));
            request
                .response
                .send(super::BackgroundControlResponse {
                    ok: true,
                    error: None,
                    status: Some(super::BackgroundStatusSnapshot {
                        kind: super::BACKGROUND_TASK_KIND_AUTO_PICK.to_string(),
                        pid: 42,
                        controller: DEFAULT_CONTROLLER.to_string(),
                        config_path: PathBuf::from("config.json"),
                        max_concurrency: 4,
                        started_at_unix: 1,
                        status_generation: 7,
                        worker_status: "running".to_string(),
                        updated_at_unix: 2,
                        auto_pick_enabled: true,
                        auto_pick_selector: Some("select".to_string()),
                        filter: "香港".to_string(),
                        latency: None,
                    }),
                })
                .expect("response sends");
        });

        let snapshot = super::send_background_control_request(
            &addr.to_string(),
            "secret",
            super::BackgroundWorkerCommand::Status,
        )
        .expect("status request succeeds");

        assert_eq!(snapshot.pid, 42);
        assert_eq!(snapshot.status_generation, 7);
        assert_eq!(snapshot.filter, "香港");
        worker.join().expect("worker joins");
    }

    #[test]
    fn background_status_poll_starts_without_blocking_the_caller() {
        let (addr, rx) = super::spawn_background_tcp_server("127.0.0.1:0", "secret".to_string())
            .expect("tcp server starts");
        let responder = thread::spawn(move || {
            let request = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("request received");
            thread::sleep(Duration::from_millis(300));
            request
                .response
                .send(super::BackgroundControlResponse {
                    ok: true,
                    error: None,
                    status: Some(super::BackgroundStatusSnapshot {
                        kind: super::BACKGROUND_TASK_KIND_AUTO_PICK.to_string(),
                        pid: 42,
                        controller: DEFAULT_CONTROLLER.to_string(),
                        config_path: PathBuf::from("config.json"),
                        max_concurrency: 4,
                        started_at_unix: 1,
                        status_generation: 7,
                        worker_status: "running".to_string(),
                        updated_at_unix: 2,
                        auto_pick_enabled: true,
                        auto_pick_selector: Some("select".to_string()),
                        filter: "香港".to_string(),
                        latency: None,
                    }),
                })
                .expect("response sends");
        });

        let started = Instant::now();
        let job = super::spawn_background_status_poll(super::BackgroundStatusTarget {
            pid: std::process::id(),
            bind_addr: addr.to_string(),
            token: "secret".to_string(),
        });
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "starting the poll must not wait for the response"
        );
        let outcome = job
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("poll completes");
        assert!(outcome.result.is_ok());
        assert!(outcome.process_alive);
        job.worker.join().expect("poll worker joins");
        responder.join().expect("responder joins");
    }

    #[test]
    fn background_tcp_control_rejects_wrong_token() {
        let (addr, _rx) = super::spawn_background_tcp_server("127.0.0.1:0", "secret".to_string())
            .expect("tcp server starts");

        let error = super::send_background_control_request(
            &addr.to_string(),
            "wrong",
            super::BackgroundWorkerCommand::Status,
        )
        .expect_err("wrong token is rejected");

        assert!(format!("{error:#}").contains("unauthorized"), "{error:#}");
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
    fn auto_select_keeps_current_when_latency_is_under_threshold() {
        let app = test_app();
        let group = ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("美国-a".to_string()),
            members: vec!["美国-a".to_string(), "美国-b".to_string()],
        };
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("美国-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![
                BenchmarkResult {
                    name: "美国-a".to_string(),
                    delay: Some(500),
                    completed: true,
                },
                BenchmarkResult {
                    name: "美国-b".to_string(),
                    delay: Some(80),
                    completed: true,
                },
            ],
        };

        assert_eq!(app.auto_select_target(&group, &summary), None);
    }

    #[test]
    fn auto_select_switches_to_best_when_current_latency_is_high() {
        let app = test_app();
        let group = ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("美国-a".to_string()),
            members: vec!["美国-a".to_string(), "美国-b".to_string()],
        };
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("美国-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![
                BenchmarkResult {
                    name: "美国-a".to_string(),
                    delay: Some(650),
                    completed: true,
                },
                BenchmarkResult {
                    name: "美国-b".to_string(),
                    delay: Some(80),
                    completed: true,
                },
            ],
        };

        assert_eq!(
            app.auto_select_target(&group, &summary),
            Some("美国-b".to_string())
        );
    }

    #[test]
    fn auto_select_switches_to_best_when_current_is_outside_filter() {
        let app = test_app();
        let group = ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("香港-a".to_string()),
            members: vec!["香港-a".to_string(), "美国-b".to_string()],
        };
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("香港-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![BenchmarkResult {
                name: "美国-b".to_string(),
                delay: Some(80),
                completed: true,
            }],
        };

        assert_eq!(
            app.auto_select_target(&group, &summary),
            Some("美国-b".to_string())
        );
    }

    #[test]
    fn auto_select_ignores_stale_results_outside_filter() {
        let app = test_app();
        let group = ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some("hk-a".to_string()),
            members: vec!["hk-a".to_string(), "us-b".to_string()],
        };
        let summary = BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("hk-a".to_string()),
            pattern: "us".to_string(),
            url: "https://www.gstatic.com/generate_204".to_string(),
            timeout_ms: 5000,
            max_concurrency: 4,
            results: vec![
                BenchmarkResult {
                    name: "hk-a".to_string(),
                    delay: Some(10),
                    completed: true,
                },
                BenchmarkResult {
                    name: "us-b".to_string(),
                    delay: Some(80),
                    completed: true,
                },
            ],
        };

        assert_eq!(
            app.auto_select_target(&group, &summary),
            Some("us-b".to_string())
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
            AutoSelectSwitchPlan {
                target_node: None,
                parent_switch: Some(("手动选择".to_string(), "AirTCP".to_string())),
            }
        );
    }

    #[test]
    fn auto_select_benchmark_waits_for_interval() {
        let mut app = test_app();
        app.auto_select_enabled = true;
        let now = Instant::now();
        app.last_auto_select_benchmark = Some(now - Duration::from_secs(29));

        assert!(!app.auto_select_benchmark_due(now));

        app.last_auto_select_benchmark = Some(now - Duration::from_secs(30));
        assert!(app.auto_select_benchmark_due(now));

        app.benchmark_filter.clear();
        assert!(app.auto_select_benchmark_due(now));
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

        let profile = PrivateAccessProfileRuntime::from_state(PrivateAccessProfileState {
            id: "sonicwall".to_string(),
            server: Some("sslvpn.example.com".to_string()),
            username: Some("alice".to_string()),
            password: Some("static-secret".to_string()),
            password_env: Some("SONICWALL_PASSWORD".to_string()),
            ..PrivateAccessProfileState::default()
        })
        .expect("SonicWall profile loads");

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
            private_access_auth_initial_value(&profile, &username_field),
            "alice"
        );
        assert_eq!(
            private_access_auth_initial_value(&profile, &password_field),
            "static-secret"
        );
        assert_eq!(
            private_access_auth_initial_value(&profile, &secret_field),
            ""
        );

        let state = profile.runtime_state();
        assert_eq!(state.username.as_deref(), Some("alice"));
        assert_eq!(state.password.as_deref(), Some("static-secret"));
        assert_eq!(state.password_env.as_deref(), Some("SONICWALL_PASSWORD"));
    }
}
