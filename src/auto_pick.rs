use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::benchmark_workflow::{QUALITY_RUNTIME_RECEIPT_ENV, QualityRuntimeReceipt};
use crate::controller::{BenchmarkSummary, ProxyGroup, matches_filter};
use crate::process_command::{command_program_name_matches, command_tokens};
use crate::process_inspection::process_is_alive as process_exists;

pub(crate) const BACKGROUND_TASK_KIND: &str = "headless-auto-pick";
const BACKGROUND_TASK_PATH: &str = "sing-box-tui-background.json";
const BACKGROUND_REGISTRY_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKGROUND_STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AutoPickConfig {
    pub(crate) enabled: bool,
    pub(crate) selector: Option<String>,
    pub(crate) filter: String,
    pub(crate) benchmark_url: String,
    pub(crate) timeout_ms: u64,
    pub(crate) request_timeout: f64,
    pub(crate) max_concurrency: usize,
    pub(crate) threshold_ms: u64,
    pub(crate) interval_secs: u64,
}

impl AutoPickConfig {
    pub(crate) fn scope_label(&self) -> String {
        if self.filter.is_empty() {
            "all nodes".to_string()
        } else {
            format!("filter '{}'", self.filter)
        }
    }

    pub(crate) fn benchmark_due(&self, last_benchmark: Option<Instant>, now: Instant) -> bool {
        self.enabled
            && last_benchmark.is_none_or(|last| {
                now.duration_since(last) >= Duration::from_secs(self.interval_secs)
            })
    }

    pub(crate) fn switch_decision(
        &self,
        group: &ProxyGroup,
        summary: &BenchmarkSummary,
        parent_switch: Option<(String, String)>,
    ) -> AutoPickDecision {
        let target_node = summary.best_success_matching_filter().and_then(|best| {
            let current = group.current.as_deref();
            let current_matches_filter =
                current.is_some_and(|name| matches_filter(name, &summary.pattern));
            let current_is_acceptable = current_matches_filter
                && current
                    .and_then(|name| summary.find_result(name))
                    .and_then(|result| result.delay)
                    .is_some_and(|delay| delay <= self.threshold_ms);
            (!current_is_acceptable && current != Some(best.name.as_str()))
                .then(|| best.name.clone())
        });
        AutoPickDecision {
            target_node,
            parent_switch,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AutoPickDecision {
    pub(crate) target_node: Option<String>,
    pub(crate) parent_switch: Option<(String, String)>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct BackgroundStatusSnapshot {
    pub(crate) kind: String,
    pub(crate) pid: u32,
    pub(crate) controller: String,
    pub(crate) config_path: PathBuf,
    pub(crate) quality_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) managed_pid: Option<u32>,
    pub(crate) max_concurrency: usize,
    pub(crate) started_at_unix: u64,
    pub(crate) status_generation: u64,
    pub(crate) worker_status: String,
    pub(crate) updated_at_unix: u64,
    pub(crate) auto_pick_enabled: bool,
    pub(crate) auto_pick_selector: Option<String>,
    pub(crate) filter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) latency: Option<BackgroundLatencySnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct BackgroundLatencySnapshot {
    pub(crate) quality_generation: u64,
    pub(crate) selector: String,
    pub(crate) current: Option<String>,
    pub(crate) pattern: String,
    pub(crate) url: String,
    pub(crate) timeout_ms: u64,
    pub(crate) max_concurrency: usize,
    pub(crate) results: Vec<BackgroundLatencyResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct BackgroundLatencyResult {
    pub(crate) name: String,
    pub(crate) delay: Option<u64>,
    pub(crate) completed: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct HeadlessWorkerMetadata {
    controller: String,
    config_path: PathBuf,
    max_concurrency: usize,
    started_at_unix: u64,
}

impl HeadlessWorkerMetadata {
    pub(crate) fn new(
        controller: impl Into<String>,
        config_path: PathBuf,
        max_concurrency: usize,
        started_at_unix: u64,
    ) -> Self {
        Self {
            controller: controller.into(),
            config_path,
            max_concurrency,
            started_at_unix,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum HeadlessWorkerCommand {
    Status,
    ApplyConfig(AutoPickConfig),
    Stop,
}

pub(crate) struct HeadlessWorkerRequest {
    pub(crate) command: HeadlessWorkerCommand,
    response: mpsc::Sender<BackgroundControlResponse>,
}

impl HeadlessWorkerRequest {
    pub(crate) fn respond(self, snapshot: BackgroundStatusSnapshot) {
        let _ = self.response.send(BackgroundControlResponse {
            ok: true,
            error: None,
            status: Some(snapshot),
        });
    }
}

pub(crate) struct HeadlessWorkerControl {
    requests: mpsc::Receiver<BackgroundWorkerRequest>,
}

impl HeadlessWorkerControl {
    pub(crate) fn start(metadata: HeadlessWorkerMetadata) -> Result<Self> {
        let token = background_token_from_env();
        let (bind_addr, requests) =
            spawn_background_tcp_server(&background_bind_addr(), token.clone())?;
        write_background_task_state(&BackgroundTaskState {
            version: 2,
            kind: BACKGROUND_TASK_KIND.to_string(),
            pid: std::process::id(),
            controller: metadata.controller,
            config_path: metadata.config_path,
            max_concurrency: metadata.max_concurrency,
            started_at_unix: metadata.started_at_unix,
            status_generation: 0,
            status: Some("starting".to_string()),
            updated_at_unix: Some(current_unix_timestamp()),
            bind_addr: bind_addr.to_string(),
            token,
        })?;
        Ok(Self { requests })
    }

    pub(crate) fn try_request(&self) -> Option<HeadlessWorkerRequest> {
        let request = match self.requests.try_recv() {
            Ok(request) => request,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
        };
        let command = match request.command {
            BackgroundWorkerCommand::Status => HeadlessWorkerCommand::Status,
            BackgroundWorkerCommand::ApplyConfig { config } => {
                HeadlessWorkerCommand::ApplyConfig(config)
            }
            BackgroundWorkerCommand::Stop => HeadlessWorkerCommand::Stop,
        };
        Some(HeadlessWorkerRequest {
            command,
            response: request.response,
        })
    }

    pub(crate) fn unregister(&self) {
        remove_background_task_state_file();
    }
}

pub(crate) fn registered_status_value() -> Result<Value> {
    let Some(state) = read_background_task_state()? else {
        return Ok(json!({ "status": "none" }));
    };
    let snapshot = match send_background_control_request(
        &state.bind_addr,
        &state.token,
        BackgroundWorkerCommand::Status,
    ) {
        Ok(snapshot) => snapshot,
        Err(_) if !process_exists(state.pid) => {
            remove_background_task_state_file();
            return Ok(json!({
                "status": "stale",
                "kind": state.kind,
                "pid": state.pid,
            }));
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
    Ok(value)
}

pub(crate) fn stop_registered_worker() -> Result<Option<u32>> {
    stop_registered_background_auto_pick_task()
}

#[derive(Clone, Debug)]
pub(crate) struct BackgroundLaunchSpec {
    controller: String,
    config_path: PathBuf,
    max_concurrency: usize,
    runtime_receipt: QualityRuntimeReceipt,
}

impl BackgroundLaunchSpec {
    pub(crate) fn new(
        controller: impl Into<String>,
        config_path: PathBuf,
        max_concurrency: usize,
        runtime_receipt: QualityRuntimeReceipt,
    ) -> Self {
        Self {
            controller: controller.into(),
            config_path,
            max_concurrency,
            runtime_receipt,
        }
    }

    fn matches_snapshot(&self, snapshot: &BackgroundStatusSnapshot) -> bool {
        snapshot.controller.trim_end_matches('/') == self.controller.trim_end_matches('/')
            && snapshot.config_path == self.config_path
            && snapshot.quality_generation == self.runtime_receipt.quality_generation()
            && snapshot.managed_pid == self.runtime_receipt.managed_pid()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BackgroundWorkerEnsure {
    AlreadyRunning(u32),
    Started(u32),
}

impl BackgroundWorkerEnsure {
    pub(crate) fn pid(&self) -> u32 {
        match self {
            Self::AlreadyRunning(pid) | Self::Started(pid) => *pid,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::AlreadyRunning(_) => "running",
            Self::Started(_) => "started",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BackgroundWorkerUpdate {
    pub(crate) latency: Option<BackgroundLatencySnapshot>,
    pub(crate) status: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum BackgroundPollEvent {
    Update(BackgroundWorkerUpdate),
    Retry(String),
    Exited(String),
    Restarted(BackgroundWorkerEnsure),
    Ensured(BackgroundWorkerEnsure),
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

struct BackgroundStatusPollJob {
    target: BackgroundStatusTarget,
    receiver: mpsc::Receiver<BackgroundStatusPollOutcome>,
    worker: JoinHandle<()>,
}

enum BackgroundStatusPollResolution {
    Snapshot(Box<BackgroundStatusSnapshot>),
    Retry(String),
    Reconnect(String),
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

pub(crate) struct BackgroundAutoPickManager {
    runtime: Option<BackgroundWorkerRuntime>,
    status_job: Option<BackgroundStatusPollJob>,
    last_status_refresh: Instant,
    last_status_generation: u64,
}

impl Default for BackgroundAutoPickManager {
    fn default() -> Self {
        Self {
            runtime: None,
            status_job: None,
            last_status_refresh: Instant::now() - BACKGROUND_STATUS_REFRESH_INTERVAL,
            last_status_generation: 0,
        }
    }
}

impl BackgroundAutoPickManager {
    pub(crate) fn ensure(
        &mut self,
        config: &AutoPickConfig,
        launch: &BackgroundLaunchSpec,
    ) -> Result<BackgroundWorkerEnsure> {
        if let Some(target) = self.runtime.as_ref().map(|runtime| BackgroundStatusTarget {
            pid: runtime.pid,
            bind_addr: runtime.bind_addr.clone(),
            token: runtime.token.clone(),
        }) {
            match send_background_control_request(
                &target.bind_addr,
                &target.token,
                BackgroundWorkerCommand::ApplyConfig {
                    config: config.clone(),
                },
            ) {
                Ok(snapshot) if launch.matches_snapshot(&snapshot) => {
                    return Ok(BackgroundWorkerEnsure::AlreadyRunning(target.pid));
                }
                Ok(_) => {
                    return self.restart_stale_target(&target, config, launch);
                }
                Err(error) if process_exists(target.pid) => {
                    return Err(error).with_context(|| {
                        format!(
                            "background auto-pick worker {} is alive but its control channel is unavailable",
                            target.pid
                        )
                    });
                }
                Err(_) => {}
            }
            self.runtime = None;
        }

        if let Some(state) = read_background_task_state()? {
            match send_background_control_request(
                &state.bind_addr,
                &state.token,
                BackgroundWorkerCommand::ApplyConfig {
                    config: config.clone(),
                },
            ) {
                Ok(snapshot) if launch.matches_snapshot(&snapshot) => {
                    self.runtime = Some(BackgroundWorkerRuntime {
                        pid: state.pid,
                        bind_addr: state.bind_addr,
                        token: state.token,
                        child: None,
                    });
                    return Ok(BackgroundWorkerEnsure::AlreadyRunning(state.pid));
                }
                Ok(_) => {
                    let target = BackgroundStatusTarget {
                        pid: state.pid,
                        bind_addr: state.bind_addr,
                        token: state.token,
                    };
                    return self.restart_stale_target(&target, config, launch);
                }
                Err(error) if process_exists(state.pid) => {
                    return Err(error).with_context(|| {
                        format!(
                            "registered background auto-pick worker {} is alive but its control channel is unavailable",
                            state.pid
                        )
                    });
                }
                Err(_) => remove_background_task_state_file(),
            }
        }

        self.spawn(config, launch)
            .map(BackgroundWorkerEnsure::Started)
    }

    pub(crate) fn poll(
        &mut self,
        enabled: bool,
        config: &AutoPickConfig,
        launch: &BackgroundLaunchSpec,
    ) -> Result<Option<BackgroundPollEvent>> {
        if let Some(job) = self.status_job.as_ref() {
            let outcome = match job.receiver.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(BackgroundStatusPollOutcome {
                    result: Err("background status poll thread disconnected".to_string()),
                    process_alive: true,
                }),
            };
            if let Some(outcome) = outcome {
                let job = self.status_job.take().expect("status poll exists");
                let target = job.target;
                let _ = job.worker.join();
                if self.current_target()?.as_ref() == Some(&target) {
                    let event = match resolve_background_status_poll(outcome) {
                        BackgroundStatusPollResolution::Snapshot(snapshot) => {
                            if !launch.matches_snapshot(&snapshot) {
                                return Ok(Some(BackgroundPollEvent::Restarted(
                                    self.restart_stale_target(&target, config, launch)?,
                                )));
                            }
                            if self.runtime.is_none() {
                                self.runtime = Some(BackgroundWorkerRuntime {
                                    pid: target.pid,
                                    bind_addr: target.bind_addr,
                                    token: target.token,
                                    child: None,
                                });
                            }
                            let status = (snapshot.status_generation > self.last_status_generation)
                                .then(|| {
                                    self.last_status_generation = snapshot.status_generation;
                                    snapshot.worker_status.clone()
                                });
                            BackgroundPollEvent::Update(BackgroundWorkerUpdate {
                                latency: snapshot.latency,
                                status,
                            })
                        }
                        BackgroundStatusPollResolution::Retry(error) => {
                            BackgroundPollEvent::Retry(error)
                        }
                        BackgroundStatusPollResolution::Reconnect(error) => {
                            self.clear_failed_target(&target)?;
                            if enabled {
                                BackgroundPollEvent::Restarted(self.ensure(config, launch)?)
                            } else {
                                BackgroundPollEvent::Exited(error)
                            }
                        }
                    };
                    return Ok(Some(event));
                }
            }
        }

        if self.status_job.is_some()
            || self.last_status_refresh.elapsed() < BACKGROUND_STATUS_REFRESH_INTERVAL
        {
            return Ok(None);
        }

        if let Some(target) = self.current_target()? {
            self.last_status_refresh = Instant::now();
            self.status_job = Some(spawn_background_status_poll(target));
            Ok(None)
        } else if enabled {
            Ok(Some(BackgroundPollEvent::Ensured(
                self.ensure(config, launch)?,
            )))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        let Some(mut runtime) = self.runtime.take() else {
            stop_registered_background_auto_pick_task()?;
            return Ok(());
        };
        let _ = send_background_control_request(
            &runtime.bind_addr,
            &runtime.token,
            BackgroundWorkerCommand::Stop,
        );
        if wait_for_background_process_to_exit(runtime.pid, Duration::from_secs(3)).is_err() {
            if let Some(mut child) = runtime.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            } else {
                let _ = stop_background_pid(runtime.pid);
            }
        } else if let Some(mut child) = runtime.child.take() {
            let _ = child.wait();
        }
        remove_background_task_state_file();
        Ok(())
    }

    fn restart_stale_target(
        &mut self,
        target: &BackgroundStatusTarget,
        config: &AutoPickConfig,
        launch: &BackgroundLaunchSpec,
    ) -> Result<BackgroundWorkerEnsure> {
        if let Some(runtime) = self.runtime.as_ref() {
            if runtime.pid != target.pid
                || runtime.bind_addr != target.bind_addr
                || runtime.token != target.token
            {
                bail!("refusing to replace a background worker other than the polled target");
            }
        } else {
            self.runtime = Some(BackgroundWorkerRuntime {
                pid: target.pid,
                bind_addr: target.bind_addr.clone(),
                token: target.token.clone(),
                child: None,
            });
        }
        // A changed managed PID invalidates the child process's runtime proof permanently. Stop
        // the authenticated old target before spawning so retries cannot leave two selectors.
        self.stop()?;
        self.spawn(config, launch)
            .map(BackgroundWorkerEnsure::Started)
    }

    fn current_target(&self) -> Result<Option<BackgroundStatusTarget>> {
        if let Some(runtime) = self.runtime.as_ref() {
            return Ok(Some(BackgroundStatusTarget {
                pid: runtime.pid,
                bind_addr: runtime.bind_addr.clone(),
                token: runtime.token.clone(),
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

    fn clear_failed_target(&mut self, target: &BackgroundStatusTarget) -> Result<()> {
        if self
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.pid == target.pid)
        {
            let mut runtime = self.runtime.take().expect("runtime exists");
            if let Some(child) = runtime.child.as_mut() {
                let _ = child.try_wait();
            }
        }
        if read_background_task_state()?.is_some_and(|state| state.pid == target.pid) {
            remove_background_task_state_file();
        }
        Ok(())
    }

    fn spawn(&mut self, config: &AutoPickConfig, launch: &BackgroundLaunchSpec) -> Result<u32> {
        let executable = env::current_exe().context("failed to locate current executable")?;
        let encoded_receipt = launch.runtime_receipt.encode_for_child()?;
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
        let mut child = Command::new(executable)
            .arg("run")
            .arg("--headless-auto-pick")
            .arg("--controller")
            .arg(&launch.controller)
            .arg("--max-concurrency")
            .arg(launch.max_concurrency.to_string())
            .arg("--config")
            .arg(&launch.config_path)
            .arg("--no-subscription-refresh")
            .env("SING_BOX_TUI_BACKGROUND_TOKEN", random_background_token())
            .env(QUALITY_RUNTIME_RECEIPT_ENV, encoded_receipt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
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
        send_background_control_request(
            &state.bind_addr,
            &state.token,
            BackgroundWorkerCommand::ApplyConfig {
                config: config.clone(),
            },
        )
        .with_context(|| {
            format!(
                "failed to apply initial background auto-pick config{}",
                background_log_tail_context(&log_path)
            )
        })?;
        self.runtime = Some(BackgroundWorkerRuntime {
            pid,
            bind_addr: state.bind_addr,
            token: state.token,
            child: Some(child),
        });
        // Status generations are local to one worker process; carrying the old counter across a
        // receipt-driven restart could suppress every early status from the replacement.
        self.last_status_generation = 0;
        Ok(pid)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BackgroundWorkerCommand {
    Status,
    ApplyConfig { config: AutoPickConfig },
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

struct BackgroundWorkerRequest {
    command: BackgroundWorkerCommand,
    response: mpsc::Sender<BackgroundControlResponse>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BackgroundTaskState {
    version: u8,
    kind: String,
    pid: u32,
    controller: String,
    config_path: PathBuf,
    max_concurrency: usize,
    started_at_unix: u64,
    #[serde(default)]
    status_generation: u64,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    updated_at_unix: Option<u64>,
    bind_addr: String,
    token: String,
}

pub(crate) fn background_task_state_path() -> PathBuf {
    env::var("SING_BOX_TUI_BACKGROUND")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(BACKGROUND_TASK_PATH))
}

pub(crate) fn background_task_log_path() -> PathBuf {
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
    if state.kind != BACKGROUND_TASK_KIND {
        bail!(
            "unsupported background task kind '{}' in {}",
            state.kind,
            path.display()
        );
    }
    Ok(Some(state))
}

fn remove_background_task_state_file() {
    let _ = fs::remove_file(background_task_state_path());
}

fn write_background_task_state(state: &BackgroundTaskState) -> Result<()> {
    write_background_task_state_to_path(&background_task_state_path(), state)
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
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        bail!(
            "refusing to write background task state through symlink {}",
            path.display()
        );
    }
    let text =
        serde_json::to_string_pretty(state).context("failed to encode background task state")?;
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
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
        return write_background_control_response(
            &mut stream,
            &BackgroundControlResponse {
                ok: false,
                error: Some("unauthorized".to_string()),
                status: None,
            },
        );
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

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

fn stop_registered_background_auto_pick_task() -> Result<Option<u32>> {
    let Some(state) = read_background_task_state()? else {
        return Ok(None);
    };
    let pid = state.pid;
    if process_exists(pid) {
        let stopped = send_background_control_request(
            &state.bind_addr,
            &state.token,
            BackgroundWorkerCommand::Stop,
        )
        .and_then(|_| wait_for_background_process_to_exit(pid, Duration::from_secs(3)))
        .is_ok();
        if !stopped {
            stop_background_pid(pid)
                .with_context(|| format!("failed to stop background auto-pick pid {pid}"))?;
        }
    }
    remove_background_task_state_file();
    Ok(Some(pid))
}

fn random_background_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_command(pid: u32) -> Result<String> {
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

fn command_matches_worker(command: &str) -> bool {
    let args = command_tokens(command);
    args.first()
        .is_some_and(|program| command_program_name_matches(program, "sing-box-tui"))
        && args.iter().any(|arg| arg == "run")
        && args.iter().any(|arg| arg == "--headless-auto-pick")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_background_pid(pid: u32) -> Result<()> {
    let command = process_command(pid)?;
    if !command_matches_worker(&command) {
        bail!("background pid {pid} is not a sing-box-tui headless auto-pick worker");
    }
    let status = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to stop background process {pid}"))?;
    if !status.success() {
        bail!("failed to stop background process {pid}: kill exited with {status}");
    }
    if wait_for_background_process_to_exit(pid, Duration::from_secs(3)).is_ok() {
        return Ok(());
    }
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .with_context(|| format!("failed to force stop background process {pid}"))?;
    if !status.success() {
        bail!("failed to force stop background process {pid}: kill -9 exited with {status}");
    }
    wait_for_background_process_to_exit(pid, Duration::from_secs(3))
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

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn wait_for_background_process_to_exit(pid: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("timed out waiting for background process {pid} to exit")
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn wait_for_background_process_to_exit(_pid: u32, _timeout: Duration) -> Result<()> {
    bail!("background worker shutdown is only available on macOS and Linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{BenchmarkResult, BenchmarkSummary};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn config() -> AutoPickConfig {
        AutoPickConfig {
            enabled: true,
            selector: Some("select".to_string()),
            filter: "美国".to_string(),
            benchmark_url: "https://example.test/ping".to_string(),
            timeout_ms: 1_000,
            request_timeout: 2.0,
            max_concurrency: 4,
            threshold_ms: 600,
            interval_secs: 30,
        }
    }

    fn group(current: &str) -> ProxyGroup {
        ProxyGroup {
            name: "select".to_string(),
            kind: "Selector".to_string(),
            current: Some(current.to_string()),
            members: vec!["美国-a".to_string(), "美国-b".to_string()],
        }
    }

    fn summary(current_delay: Option<u64>, best_delay: u64) -> BenchmarkSummary {
        BenchmarkSummary {
            selector: "select".to_string(),
            current: Some("美国-a".to_string()),
            pattern: "美国".to_string(),
            url: "https://example.test/ping".to_string(),
            timeout_ms: 1_000,
            max_concurrency: 4,
            results: vec![
                BenchmarkResult {
                    name: "美国-a".to_string(),
                    delay: current_delay,
                    completed: true,
                },
                BenchmarkResult {
                    name: "美国-b".to_string(),
                    delay: Some(best_delay),
                    completed: true,
                },
            ],
        }
    }

    fn status_snapshot() -> BackgroundStatusSnapshot {
        BackgroundStatusSnapshot {
            kind: BACKGROUND_TASK_KIND.to_string(),
            pid: 42,
            controller: "http://127.0.0.1:9992".to_string(),
            config_path: PathBuf::from("config.json"),
            quality_generation: 0,
            managed_pid: None,
            max_concurrency: 4,
            started_at_unix: 1,
            status_generation: 7,
            worker_status: "running".to_string(),
            updated_at_unix: 2,
            auto_pick_enabled: true,
            auto_pick_selector: Some("select".to_string()),
            filter: "香港".to_string(),
            latency: None,
        }
    }

    fn task_state() -> BackgroundTaskState {
        BackgroundTaskState {
            version: 2,
            kind: BACKGROUND_TASK_KIND.to_string(),
            pid: 42,
            controller: "http://127.0.0.1:9992".to_string(),
            config_path: PathBuf::from("config.json"),
            max_concurrency: 16,
            started_at_unix: 1,
            status_generation: 0,
            status: Some("starting".to_string()),
            updated_at_unix: Some(1),
            bind_addr: "127.0.0.1:9999".to_string(),
            token: "secret".to_string(),
        }
    }

    fn test_path(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "sing-box-tui-{label}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[test]
    fn decision_keeps_a_healthy_current_node() {
        let decision = config().switch_decision(&group("美国-a"), &summary(Some(90), 40), None);
        assert_eq!(
            decision,
            AutoPickDecision {
                target_node: None,
                parent_switch: None,
            }
        );
    }

    #[test]
    fn decision_switches_a_failed_or_slow_current_node() {
        let slow = config().switch_decision(&group("美国-a"), &summary(Some(700), 40), None);
        assert_eq!(slow.target_node.as_deref(), Some("美国-b"));

        let failed = config().switch_decision(&group("美国-a"), &summary(None, 40), None);
        assert_eq!(failed.target_node.as_deref(), Some("美国-b"));
    }

    #[test]
    fn decision_switches_a_current_node_outside_the_filter() {
        let decision = config().switch_decision(&group("日本-a"), &summary(Some(90), 40), None);
        assert_eq!(decision.target_node.as_deref(), Some("美国-b"));
    }

    #[test]
    fn decision_keeps_parent_route_selection_independent_from_node_switch() {
        let decision = config().switch_decision(
            &group("美国-a"),
            &summary(Some(90), 40),
            Some(("root".to_string(), "select".to_string())),
        );
        assert_eq!(decision.target_node, None);
        assert_eq!(
            decision.parent_switch,
            Some(("root".to_string(), "select".to_string()))
        );
    }

    #[test]
    fn due_schedule_is_shared_by_foreground_and_headless_runs() {
        let now = Instant::now();
        let policy = config();
        assert!(policy.benchmark_due(None, now));
        assert!(!policy.benchmark_due(Some(now - Duration::from_secs(29)), now));
        assert!(policy.benchmark_due(Some(now - Duration::from_secs(30)), now));
    }

    #[test]
    fn poll_failure_retries_live_workers_and_reconnects_exited_workers() {
        let retry = resolve_background_status_poll(BackgroundStatusPollOutcome {
            result: Err("temporary TCP timeout".to_string()),
            process_alive: true,
        });
        assert!(matches!(
            retry,
            BackgroundStatusPollResolution::Retry(error)
                if error == "temporary TCP timeout"
        ));

        let reconnect = resolve_background_status_poll(BackgroundStatusPollOutcome {
            result: Err("worker exited".to_string()),
            process_alive: false,
        });
        assert!(matches!(
            reconnect,
            BackgroundStatusPollResolution::Reconnect(error)
                if error == "worker exited"
        ));
    }

    #[test]
    fn changed_managed_pid_requires_background_worker_replacement() {
        let receipt = QualityRuntimeReceipt::decode_from_child(
            r#"{"canonical_config_path":"config.json","canonical_database_path":"quality.sqlite3","controller_base_url":"http://127.0.0.1:9992","quality_generation":7,"managed_pid":100}"#,
        )
        .expect("test receipt decodes");
        let launch = BackgroundLaunchSpec::new(
            "http://127.0.0.1:9992",
            PathBuf::from("config.json"),
            4,
            receipt,
        );
        let mut snapshot = status_snapshot();
        snapshot.config_path = PathBuf::from("config.json");
        snapshot.quality_generation = 7;
        snapshot.managed_pid = Some(100);
        assert!(launch.matches_snapshot(&snapshot));

        snapshot.managed_pid = Some(101);
        assert!(
            !launch.matches_snapshot(&snapshot),
            "a new managed sing-box process must enter the stop-and-spawn path"
        );
    }

    #[test]
    fn worker_command_matcher_handles_platform_paths_and_rejects_other_processes() {
        assert!(command_matches_worker(
            "/opt/sing-box-tui run --headless-auto-pick --controller http://127.0.0.1:9992"
        ));
        assert!(command_matches_worker(
            r#""C:\Program Files\sing-box-tui\sing-box-tui.exe" run --headless-auto-pick"#
        ));
        assert!(!command_matches_worker("sing-box-tui run"));
        assert!(!command_matches_worker("sing-box run --headless-auto-pick"));
    }

    #[test]
    fn remote_control_bind_requires_explicit_allow() {
        assert!(validate_background_bind_addr_with_remote("127.0.0.1:0", false).is_ok());
        assert!(validate_background_bind_addr_with_remote("[::1]:0", false).is_ok());
        let error = validate_background_bind_addr_with_remote("0.0.0.0:9999", false)
            .expect_err("remote bind requires explicit allow");
        assert!(format!("{error:#}").contains("refusing non-loopback"));
        assert!(validate_background_bind_addr_with_remote("0.0.0.0:9999", true).is_ok());
    }

    #[test]
    fn log_tail_handles_missing_empty_and_truncated_files() {
        let missing = test_path("missing-log");
        assert_eq!(read_text_tail(&missing, 16), None);

        let path = test_path("tail-log");
        fs::write(&path, "").expect("empty log writes");
        assert_eq!(read_text_tail(&path, 16), None);
        fs::write(&path, "first\nsecond\n").expect("small log writes");
        assert_eq!(
            read_text_tail(&path, 1024).as_deref(),
            Some("first\nsecond")
        );
        fs::write(&path, "0123456789abcdef").expect("large log writes");
        assert_eq!(read_text_tail(&path, 6).as_deref(), Some("abcdef"));
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn registry_is_private_and_refuses_symlinks() {
        let path = test_path("background-state");
        write_background_task_state_to_path(&path, &task_state()).expect("state writes");
        let mode = fs::metadata(&path)
            .expect("state metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let target = test_path("background-target");
        let link = test_path("background-link");
        fs::write(&target, "{}").expect("target writes");
        std::os::unix::fs::symlink(&target, &link).expect("symlink writes");
        let error = write_background_task_state_to_path(&link, &task_state())
            .expect_err("symlink is rejected");
        assert!(format!("{error:#}").contains("refusing to write"));

        let _ = fs::remove_file(path);
        let _ = fs::remove_file(link);
        let _ = fs::remove_file(target);
    }

    #[test]
    fn headless_control_round_trips_status_through_its_public_request_type() {
        let (addr, requests) = spawn_background_tcp_server("127.0.0.1:0", "secret".to_string())
            .expect("TCP server starts");
        let control = HeadlessWorkerControl { requests };
        let client = thread::spawn(move || {
            send_background_control_request(
                &addr.to_string(),
                "secret",
                BackgroundWorkerCommand::Status,
            )
            .expect("status request succeeds")
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let request = loop {
            if let Some(request) = control.try_request() {
                break request;
            }
            assert!(Instant::now() < deadline, "request timed out");
            thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(request.command, HeadlessWorkerCommand::Status));
        request.respond(status_snapshot());

        let snapshot = client.join().expect("client joins");
        assert_eq!(snapshot.pid, 42);
        assert_eq!(snapshot.status_generation, 7);
        assert_eq!(snapshot.filter, "香港");
    }

    #[test]
    fn status_poll_starts_without_blocking_the_caller() {
        let (addr, requests) = spawn_background_tcp_server("127.0.0.1:0", "secret".to_string())
            .expect("TCP server starts");
        let responder = thread::spawn(move || {
            let request = requests
                .recv_timeout(Duration::from_secs(2))
                .expect("request received");
            thread::sleep(Duration::from_millis(300));
            request
                .response
                .send(BackgroundControlResponse {
                    ok: true,
                    error: None,
                    status: Some(status_snapshot()),
                })
                .expect("response sends");
        });

        let started = Instant::now();
        let job = spawn_background_status_poll(BackgroundStatusTarget {
            pid: std::process::id(),
            bind_addr: addr.to_string(),
            token: "secret".to_string(),
        });
        assert!(started.elapsed() < Duration::from_millis(200));
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
    fn control_rejects_a_wrong_token() {
        let (addr, _requests) = spawn_background_tcp_server("127.0.0.1:0", "secret".to_string())
            .expect("TCP server starts");
        let error = send_background_control_request(
            &addr.to_string(),
            "wrong",
            BackgroundWorkerCommand::Status,
        )
        .expect_err("wrong token is rejected");
        assert!(format!("{error:#}").contains("unauthorized"));
    }
}
