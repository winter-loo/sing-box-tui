use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rand::Rng;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::config::parse_sing_box_config_text;
use crate::controller::ApiClient;
use crate::managed_sing_box::{managed_sing_box_process_matches, resolve_sing_box_executable};
use crate::process_inspection::process_is_alive;

const DEFAULT_CONNECTIVITY_TIMEOUT_MS: u64 = 2_000;
const MIN_CONNECTIVITY_TIMEOUT_MS: u64 = 500;
const MAX_CONNECTIVITY_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_MAX_RUNTIMES: usize = 4;
const ACTIVE_ENVIRONMENT_DIR: &str = "sing-box-tui-active-environments";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct NodeDescriptor {
    tag: String,
    selectors: Vec<String>,
}

fn enumerate_nodes(config: &Value) -> Result<Vec<NodeDescriptor>> {
    let outbounds = config
        .get("outbounds")
        .and_then(Value::as_array)
        .context("sing-box config must contain an outbounds array")?;
    let by_tag = outbounds
        .iter()
        .filter_map(|outbound| {
            outbound
                .get("tag")
                .and_then(Value::as_str)
                .map(|tag| (tag, outbound))
        })
        .collect::<HashMap<_, _>>();
    let selector_tags = outbounds
        .iter()
        .filter(|outbound| outbound.get("type").and_then(Value::as_str) == Some("selector"))
        .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
        .collect::<Vec<_>>();

    let mut order = Vec::<String>::new();
    let mut selectors_by_node = HashMap::<String, Vec<String>>::new();
    for root in selector_tags {
        let mut stack = Vec::new();
        expand_selector_member(
            root,
            &by_tag,
            &mut stack,
            &mut order,
            &mut selectors_by_node,
        )?;
    }

    Ok(order
        .into_iter()
        .map(|tag| NodeDescriptor {
            selectors: selectors_by_node.remove(&tag).unwrap_or_default(),
            tag,
        })
        .collect())
}

fn expand_selector_member<'a>(
    tag: &'a str,
    by_tag: &HashMap<&'a str, &'a Value>,
    selector_stack: &mut Vec<&'a str>,
    order: &mut Vec<String>,
    selectors_by_node: &mut HashMap<String, Vec<String>>,
) -> Result<()> {
    let outbound = by_tag
        .get(tag)
        .copied()
        .with_context(|| format!("selector references missing outbound {tag}"))?;
    let kind = outbound.get("type").and_then(Value::as_str).unwrap_or("");
    if matches!(kind, "selector" | "urltest") {
        if selector_stack.contains(&tag) {
            bail!("selector graph contains a cycle at {tag}");
        }
        selector_stack.push(tag);
        let members = outbound
            .get("outbounds")
            .and_then(Value::as_array)
            .with_context(|| format!("{kind} outbound {tag} must contain an outbounds array"))?;
        for member in members {
            let member = member
                .as_str()
                .with_context(|| format!("{kind} outbound {tag} has a non-string member"))?;
            expand_selector_member(member, by_tag, selector_stack, order, selectors_by_node)?;
        }
        selector_stack.pop();
        return Ok(());
    }
    if matches!(kind, "direct" | "block" | "dns") {
        return Ok(());
    }

    if !selectors_by_node.contains_key(tag) {
        order.push(tag.to_string());
    }
    let memberships = selectors_by_node.entry(tag.to_string()).or_default();
    for selector in selector_stack.iter().copied() {
        if by_tag
            .get(selector)
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            == Some("selector")
            && !memberships.iter().any(|existing| existing == selector)
        {
            memberships.push(selector.to_string());
        }
    }
    Ok(())
}

pub(crate) fn run_stdio() -> Result<()> {
    let manager = Arc::new(NodeRuntimeManager::default());
    let (event_tx, event_rx) = mpsc::channel::<StdioEvent>();
    let reader_tx = event_tx.clone();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    if reader_tx.send(StdioEvent::Request(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = reader_tx.send(StdioEvent::InputError(error.to_string()));
                    return;
                }
            }
        }
        let _ = reader_tx.send(StdioEvent::Eof);
    });

    let mut output = io::stdout().lock();
    let mut pending = 0_usize;
    let mut input_closed = false;
    while let Ok(event) = event_rx.recv() {
        match event {
            StdioEvent::Request(line) => {
                pending += 1;
                let manager = Arc::clone(&manager);
                let response_tx = event_tx.clone();
                thread::spawn(move || {
                    let (response, completed_next) = match serde_json::from_str::<RpcRequest>(&line)
                    {
                        Ok(request) => {
                            let completed_next = (request.method == "next")
                                .then(|| {
                                    request
                                        .params
                                        .get("runtime_id")
                                        .and_then(Value::as_str)
                                        .map(str::to_string)
                                })
                                .flatten();
                            (manager.handle(request), completed_next)
                        }
                        Err(error) => (
                            RpcResponse::error(
                                Value::Null,
                                "invalid_request",
                                format!("invalid JSON request: {error}"),
                            ),
                            None,
                        ),
                    };
                    let _ = response_tx.send(StdioEvent::Response {
                        response,
                        completed_next,
                    });
                });
            }
            StdioEvent::Response {
                response,
                completed_next,
            } => {
                let next_was_rejected_as_busy = response
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "next_in_progress");
                let encoded = serde_json::to_string(&response)
                    .context("failed to encode node runtime manager response")?;
                writeln!(output, "{encoded}")
                    .context("failed to write node runtime manager response")?;
                output
                    .flush()
                    .context("failed to flush node runtime manager response")?;
                if !next_was_rejected_as_busy && let Some(runtime_id) = completed_next {
                    manager.complete_next(&runtime_id);
                }
                pending = pending.saturating_sub(1);
                if input_closed && pending == 0 {
                    break;
                }
            }
            StdioEvent::InputError(message) => {
                manager.cancel_all();
                return Err(anyhow!(
                    "failed to read node runtime manager request: {message}"
                ));
            }
            StdioEvent::Eof => {
                input_closed = true;
                manager.cancel_all();
                if pending == 0 {
                    break;
                }
            }
        }
    }
    manager.close_all();
    Ok(())
}

enum StdioEvent {
    Request(String),
    Response {
        response: RpcResponse,
        completed_next: Option<String>,
    },
    InputError(String),
    Eof,
}

#[derive(Debug, serde::Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: String,
    message: String,
}

impl RpcResponse {
    fn success(id: Value, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Value, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(RpcError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Default)]
struct NodeRuntimeManager {
    state: Mutex<ManagerState>,
    shutting_down: AtomicBool,
}

#[derive(Default)]
struct ManagerState {
    environment: Option<RuntimeEnvironment>,
    runtimes: HashMap<String, Arc<RuntimeHandle>>,
}

#[derive(Clone)]
struct RuntimeEnvironment {
    config_path: PathBuf,
    sing_box_executable: PathBuf,
    source: Value,
    nodes: Vec<NodeDescriptor>,
    max_runtimes: usize,
}

struct RuntimeHandle {
    cancelled: AtomicBool,
    next_in_flight: AtomicBool,
    shutdown_signal: Arc<Mutex<Option<ChildStdin>>>,
    runtime: Mutex<NodeRuntime>,
}

impl NodeRuntimeManager {
    fn handle(&self, request: RpcRequest) -> RpcResponse {
        let id = request.id;
        let outcome = match request.method.as_str() {
            "initialize" => self.initialize(&request.params),
            "create_runtime" => self.create_runtime(&request.params),
            "next" => self.next(&request.params),
            "close_runtime" => self.close_runtime(&request.params),
            method => Err(ManagerError::new(
                "method_not_found",
                format!("unknown method {method}"),
            )),
        };
        match outcome {
            Ok(result) => RpcResponse::success(id, result),
            Err(error) => RpcResponse::error(id, error.code, error.message),
        }
    }

    fn initialize(&self, params: &Value) -> ManagerResult<Value> {
        let params = object_params(params)?;
        let config_path = optional_path(params, "config_path")?;
        let executable = optional_path(params, "sing_box_executable")?;
        if config_path.is_some() != executable.is_some() {
            return Err(ManagerError::new(
                "invalid_params",
                "config_path and sing_box_executable must be supplied together",
            ));
        }
        let max_runtimes = optional_u64(params, "max_runtimes")?
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_MAX_RUNTIMES);
        if max_runtimes == 0 {
            return Err(ManagerError::new(
                "invalid_params",
                "max_runtimes must be greater than zero",
            ));
        }

        let (config_path, sing_box_executable) = match (config_path, executable) {
            (Some(config_path), Some(executable)) => (config_path, executable),
            (None, None) => discover_active_environment()?,
            _ => unreachable!(),
        };
        let config_path = config_path.canonicalize().map_err(|error| {
            ManagerError::new(
                "invalid_environment",
                format!(
                    "failed to resolve config {}: {error}",
                    config_path.display()
                ),
            )
        })?;
        let sing_box_executable =
            resolve_sing_box_executable(&sing_box_executable).map_err(|error| {
                ManagerError::new(
                    "invalid_environment",
                    format!("failed to resolve sing-box executable: {error:#}"),
                )
            })?;
        let text = fs::read_to_string(&config_path).map_err(|error| {
            ManagerError::new(
                "invalid_environment",
                format!("failed to read config {}: {error}", config_path.display()),
            )
        })?;
        let source = parse_sing_box_config_text(&text).map_err(|error| {
            ManagerError::new(
                "invalid_environment",
                format!("invalid sing-box config: {error:#}"),
            )
        })?;
        let nodes = enumerate_nodes(&source).map_err(|error| {
            ManagerError::new(
                "invalid_environment",
                format!("failed to enumerate nodes: {error:#}"),
            )
        })?;

        let mut state = self.state.lock().expect("manager mutex poisoned");
        if state.environment.is_some() {
            return Err(ManagerError::new(
                "already_initialized",
                "node runtime manager is already initialized",
            ));
        }
        state.environment = Some(RuntimeEnvironment {
            config_path: config_path.clone(),
            sing_box_executable: sing_box_executable.clone(),
            source,
            nodes: nodes.clone(),
            max_runtimes,
        });
        Ok(json!({
            "config_path": config_path,
            "sing_box_executable": sing_box_executable,
            "total_candidates": nodes.len(),
            "max_runtimes": max_runtimes,
        }))
    }

    fn create_runtime(&self, params: &Value) -> ManagerResult<Value> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(ManagerError::new(
                "manager_shutting_down",
                "node runtime manager is shutting down",
            ));
        }
        let params = object_params(params)?;
        let url = required_string(params, "url")?.to_string();
        let parsed = reqwest::Url::parse(&url).map_err(|error| {
            ManagerError::new("invalid_params", format!("invalid url: {error}"))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ManagerError::new(
                "invalid_params",
                "url must use http or https",
            ));
        }
        let timeout_ms = optional_u64(params, "connectivity_timeout_ms")?
            .unwrap_or(DEFAULT_CONNECTIVITY_TIMEOUT_MS);
        if !(MIN_CONNECTIVITY_TIMEOUT_MS..=MAX_CONNECTIVITY_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(ManagerError::new(
                "invalid_params",
                format!(
                    "connectivity_timeout_ms must be between {MIN_CONNECTIVITY_TIMEOUT_MS} and {MAX_CONNECTIVITY_TIMEOUT_MS}"
                ),
            ));
        }

        let environment = {
            let state = self.state.lock().expect("manager mutex poisoned");
            let environment = state.environment.clone().ok_or_else(|| {
                ManagerError::new("not_initialized", "initialize must be called first")
            })?;
            if state.runtimes.len() >= environment.max_runtimes {
                return Err(ManagerError::new(
                    "runtime_limit_reached",
                    format!(
                        "at most {} runtimes may be active",
                        environment.max_runtimes
                    ),
                ));
            }
            environment
        };

        let runtime_id = random_id();
        let runtime = NodeRuntime::start(runtime_id.clone(), environment, url, timeout_ms)
            .map_err(|error| ManagerError::new("runtime_failed", format!("{error:#}")))?;
        let response = runtime.creation_response();
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(ManagerError::new(
                "manager_shutting_down",
                "node runtime manager is shutting down",
            ));
        }
        let mut state = self.state.lock().expect("manager mutex poisoned");
        if state.runtimes.len()
            >= state
                .environment
                .as_ref()
                .map(|value| value.max_runtimes)
                .unwrap_or(0)
        {
            return Err(ManagerError::new(
                "runtime_limit_reached",
                "runtime limit was reached while creating the runtime",
            ));
        }
        state.runtimes.insert(
            runtime_id,
            Arc::new(RuntimeHandle {
                cancelled: AtomicBool::new(false),
                next_in_flight: AtomicBool::new(false),
                shutdown_signal: Arc::clone(&runtime.resources.shutdown_signal),
                runtime: Mutex::new(runtime),
            }),
        );
        Ok(response)
    }

    fn next(&self, params: &Value) -> ManagerResult<Value> {
        let runtime_id = required_string(object_params(params)?, "runtime_id")?;
        let handle = {
            let state = self.state.lock().expect("manager mutex poisoned");
            state.runtimes.get(runtime_id).cloned().ok_or_else(|| {
                ManagerError::new("runtime_not_found", format!("unknown runtime {runtime_id}"))
            })?
        };
        if handle
            .next_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(ManagerError::new(
                "next_in_progress",
                format!("runtime {runtime_id} already has an outstanding next request"),
            ));
        }
        let response = handle
            .runtime
            .lock()
            .expect("runtime mutex poisoned")
            .next(&handle.cancelled)
            .map_err(|error| ManagerError::new("runtime_failed", format!("{error:#}")))?;
        if response.get("end").and_then(Value::as_bool) == Some(true) {
            self.state
                .lock()
                .expect("manager mutex poisoned")
                .runtimes
                .remove(runtime_id);
        }
        Ok(response)
    }

    fn close_runtime(&self, params: &Value) -> ManagerResult<Value> {
        let runtime_id = required_string(object_params(params)?, "runtime_id")?;
        let handle = self
            .state
            .lock()
            .expect("manager mutex poisoned")
            .runtimes
            .remove(runtime_id);
        if let Some(handle) = handle {
            handle.cancelled.store(true, Ordering::SeqCst);
            handle
                .shutdown_signal
                .lock()
                .expect("shutdown signal mutex poisoned")
                .take();
            handle
                .runtime
                .lock()
                .expect("runtime mutex poisoned")
                .shutdown();
        }
        Ok(json!({"closed": true, "runtime_id": runtime_id}))
    }

    fn close_all(&self) {
        let handles = self
            .state
            .lock()
            .expect("manager mutex poisoned")
            .runtimes
            .drain()
            .map(|(_, handle)| handle)
            .collect::<Vec<_>>();
        for handle in handles {
            handle.cancelled.store(true, Ordering::SeqCst);
            handle
                .shutdown_signal
                .lock()
                .expect("shutdown signal mutex poisoned")
                .take();
            handle
                .runtime
                .lock()
                .expect("runtime mutex poisoned")
                .shutdown();
        }
    }

    fn cancel_all(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for handle in self
            .state
            .lock()
            .expect("manager mutex poisoned")
            .runtimes
            .values()
        {
            handle.cancelled.store(true, Ordering::SeqCst);
            handle
                .shutdown_signal
                .lock()
                .expect("shutdown signal mutex poisoned")
                .take();
        }
    }

    fn complete_next(&self, runtime_id: &str) {
        if let Some(handle) = self
            .state
            .lock()
            .expect("manager mutex poisoned")
            .runtimes
            .get(runtime_id)
        {
            handle.next_in_flight.store(false, Ordering::SeqCst);
        }
    }
}

struct NodeRuntime {
    id: String,
    url: String,
    timeout_ms: u64,
    nodes: Vec<NodeDescriptor>,
    cursor: usize,
    reachable: usize,
    proxy_port: u16,
    selector_tag: String,
    block_tag: String,
    controller: ApiClient,
    resources: RuntimeResources,
    poisoned: bool,
}

impl NodeRuntime {
    fn start(
        id: String,
        environment: RuntimeEnvironment,
        url: String,
        timeout_ms: u64,
    ) -> Result<Self> {
        let mut last_bind_error = None;
        for _ in 0..5 {
            match Self::start_once(id.clone(), environment.clone(), url.clone(), timeout_ms) {
                Ok(runtime) => return Ok(runtime),
                Err(error) if error_chain_reports_bind_conflict(&error) => {
                    last_bind_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_bind_error.expect("a bind retry records its error"))
    }

    fn start_once(
        id: String,
        environment: RuntimeEnvironment,
        url: String,
        timeout_ms: u64,
    ) -> Result<Self> {
        // Keep both ports reserved while the config and supervisor command are built.
        // sing-box cannot adopt pre-bound sockets, so release them at the last possible
        // moment and retry the complete startup if another process wins that small race.
        let (proxy_reservation, controller_reservation) = reserve_runtime_ports()?;
        let proxy_port = proxy_reservation.local_addr()?.port();
        let controller_port = controller_reservation.local_addr()?.port();
        let temp_dir = create_private_temp_dir(&id)?;
        let mut resources = RuntimeResources::new(temp_dir);
        let selector_tag = format!("__sing_box_tui_node_runtime_{id}");
        let block_tag = format!("{selector_tag}_block");
        let config = build_runtime_config(
            environment.source,
            &environment.nodes,
            proxy_port,
            controller_port,
            &selector_tag,
            &block_tag,
        )?;
        let config_path = resources.temp_dir.join("config.json");
        let encoded =
            serde_json::to_vec_pretty(&config).context("failed to encode runtime config")?;
        fs::write(&config_path, encoded)
            .with_context(|| format!("failed to write {}", config_path.display()))?;
        let log_path = resources.temp_dir.join("sing-box.log");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("failed to create {}", log_path.display()))?;
        let stderr = log.try_clone().context("failed to clone runtime log")?;
        let source_dir = environment
            .config_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let manager_executable =
            std::env::current_exe().context("failed to resolve node runtime manager executable")?;
        let mut command = Command::new(manager_executable);
        command
            .arg("node-runtime-child-supervisor")
            .arg("--sing-box")
            .arg(&environment.sing_box_executable)
            .arg("--config")
            .arg(&config_path)
            .arg("--directory")
            .arg(source_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(name);
        }
        drop(proxy_reservation);
        drop(controller_reservation);
        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start isolated sing-box supervisor for {}",
                environment.sing_box_executable.display()
            )
        })?;
        let child_stdin = child
            .stdin
            .take()
            .context("isolated sing-box supervisor did not expose stdin")?;
        *resources
            .shutdown_signal
            .lock()
            .expect("shutdown signal mutex poisoned") = Some(child_stdin);
        resources.child = Some(child);
        let controller = ApiClient::new(format!("http://127.0.0.1:{controller_port}"), None)?;
        let mut runtime = Self {
            id,
            url,
            timeout_ms,
            nodes: environment.nodes,
            cursor: 0,
            reachable: 0,
            proxy_port,
            selector_tag,
            block_tag,
            controller,
            resources,
            poisoned: false,
        };
        runtime.wait_until_ready(&log_path)?;
        Ok(runtime)
    }

    fn creation_response(&self) -> Value {
        json!({
            "runtime_id": self.id,
            "proxy": self.proxy_value(),
            "total_candidates": self.nodes.len(),
        })
    }

    fn next(&mut self, cancelled: &AtomicBool) -> Result<Value> {
        if self.poisoned {
            bail!("node runtime is poisoned after an infrastructure failure");
        }
        self.ensure_child_alive()?;
        if let Err(error) = self
            .controller
            .switch_proxy(&self.selector_tag, &self.block_tag)
        {
            self.poisoned = true;
            return Err(error).context("failed to disconnect the previous node");
        }
        while self.cursor < self.nodes.len() {
            if cancelled.load(Ordering::SeqCst) {
                bail!("node runtime was closed");
            }
            self.ensure_child_alive()?;
            let ordinal = self.cursor + 1;
            let node = self.nodes[self.cursor].clone();
            self.cursor += 1;
            if self
                .controller
                .measure_proxy_delay(&node.tag, &self.url, self.timeout_ms)
                .is_none()
            {
                if let Err(error) = self.controller.fetch_config() {
                    self.poisoned = true;
                    return Err(error).context("isolated sing-box controller became unavailable");
                }
                continue;
            }
            if let Err(error) = self.controller.switch_proxy(&self.selector_tag, &node.tag) {
                if self.controller.fetch_config().is_err() {
                    self.poisoned = true;
                    return Err(error).context("isolated sing-box controller became unavailable");
                }
                eprintln!(
                    "node runtime {} skipped node at ordinal {} after switch failure: {error:#}",
                    self.id, ordinal
                );
                continue;
            }
            self.reachable += 1;
            return Ok(json!({
                "end": false,
                "node": {
                    "tag": node.tag,
                    "selectors": node.selectors,
                    "ordinal": ordinal,
                },
                "proxy": self.proxy_value(),
                "scanned": self.cursor,
                "reachable": self.reachable,
            }));
        }
        let response = json!({
            "end": true,
            "scanned": self.cursor,
            "reachable": self.reachable,
        });
        self.shutdown();
        Ok(response)
    }

    fn proxy_value(&self) -> Value {
        json!({
            "http": format!("http://127.0.0.1:{}", self.proxy_port),
            "socks5": format!("socks5://127.0.0.1:{}", self.proxy_port),
        })
    }

    fn wait_until_ready(&mut self, log_path: &Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            self.ensure_child_alive().map_err(|error| {
                anyhow!(
                    "{error:#}; {}",
                    bounded_log_tail(log_path).unwrap_or_default()
                )
            })?;
            if self.controller.fetch_config().is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.poisoned = true;
                bail!(
                    "isolated sing-box controller did not become ready; {}",
                    bounded_log_tail(log_path).unwrap_or_default()
                );
            }
            thread::sleep(Duration::from_millis(75));
        }
    }

    fn ensure_child_alive(&mut self) -> Result<()> {
        let Some(child) = self.resources.child.as_mut() else {
            self.poisoned = true;
            bail!("isolated sing-box is not running");
        };
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect isolated sing-box")?
        {
            self.poisoned = true;
            bail!("isolated sing-box exited with {status}");
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.resources.shutdown();
    }
}

impl Drop for NodeRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct RuntimeResources {
    temp_dir: PathBuf,
    child: Option<Child>,
    shutdown_signal: Arc<Mutex<Option<ChildStdin>>>,
}

impl RuntimeResources {
    fn new(temp_dir: PathBuf) -> Self {
        Self {
            temp_dir,
            child: None,
            shutdown_signal: Arc::new(Mutex::new(None)),
        }
    }

    fn shutdown(&mut self) {
        self.shutdown_signal
            .lock()
            .expect("shutdown signal mutex poisoned")
            .take();
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    _ if Instant::now() >= deadline => {
                        // The supervisor owns the real sing-box process. Do not force-kill
                        // the supervisor here: stdin has already been closed, and killing
                        // it before its child cleanup completes could orphan sing-box.
                        // Keep the handle and reap it before removing runtime artifacts.
                        let _ = child.wait();
                        break;
                    }
                    _ => thread::sleep(Duration::from_millis(25)),
                }
            }
        }
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

impl Drop for RuntimeResources {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn run_child_supervisor(sing_box: &Path, config: &Path, directory: &Path) -> Result<()> {
    let mut command = Command::new(sing_box);
    command
        .arg("run")
        .arg("--disable-color")
        .arg("--directory")
        .arg(directory)
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(name);
    }
    let child = command
        .spawn()
        .with_context(|| format!("failed to start isolated sing-box {}", sing_box.display()))?;
    let mut child = SupervisedChild(Some(child));
    let (closed_tx, closed_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut input = io::stdin();
        let mut buffer = [0_u8; 64];
        while input.read(&mut buffer).is_ok_and(|count| count > 0) {}
        let _ = closed_tx.send(());
    });
    loop {
        if closed_rx.try_recv().is_ok() {
            child.terminate();
            return Ok(());
        }
        if let Some(status) = child
            .as_mut()
            .try_wait()
            .context("failed to inspect isolated sing-box")?
        {
            child.disarm();
            if status.success() {
                return Ok(());
            }
            bail!("isolated sing-box exited with {status}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

struct SupervisedChild(Option<Child>);

impl SupervisedChild {
    fn as_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("supervised child is armed")
    }

    fn disarm(&mut self) {
        self.0.take();
    }

    fn terminate(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn build_runtime_config(
    mut source: Value,
    nodes: &[NodeDescriptor],
    proxy_port: u16,
    controller_port: u16,
    selector_tag: &str,
    block_tag: &str,
) -> Result<Value> {
    let root = source
        .as_object_mut()
        .context("sing-box config root must be an object")?;
    root.insert("log".to_string(), json!({"level":"warn","timestamp":true}));
    root.insert(
        "inbounds".to_string(),
        json!([{
            "type":"mixed",
            "tag":format!("{selector_tag}_in"),
            "listen":"127.0.0.1",
            "listen_port":proxy_port
        }]),
    );
    let outbounds = root
        .entry("outbounds".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("sing-box config outbounds must be an array")?;
    outbounds.push(json!({"type":"block","tag":block_tag}));
    let mut members = vec![block_tag.to_string()];
    members.extend(nodes.iter().map(|node| node.tag.clone()));
    outbounds.push(json!({
        "type":"selector",
        "tag":selector_tag,
        "outbounds":members,
        "default":block_tag,
        "interrupt_exist_connections":true
    }));
    let source_route = root.get("route").and_then(Value::as_object);
    let default_domain_resolver = source_route
        .and_then(|route| route.get("default_domain_resolver"))
        .cloned();
    let rule_sets = source_route
        .and_then(|route| route.get("rule_set"))
        .cloned();
    let mut route = Map::new();
    route.insert("final".to_string(), json!(selector_tag));
    if let Some(default_domain_resolver) = default_domain_resolver {
        route.insert(
            "default_domain_resolver".to_string(),
            default_domain_resolver,
        );
    }
    if let Some(rule_sets) = rule_sets {
        route.insert("rule_set".to_string(), rule_sets);
    }
    root.insert("route".to_string(), Value::Object(route));
    root.insert(
        "experimental".to_string(),
        json!({"clash_api":{"external_controller":format!("127.0.0.1:{controller_port}")}}),
    );
    Ok(source)
}

fn reserve_runtime_ports() -> Result<(TcpListener, TcpListener)> {
    let proxy =
        TcpListener::bind(("127.0.0.1", 0)).context("failed to reserve runtime proxy port")?;
    let controller =
        TcpListener::bind(("127.0.0.1", 0)).context("failed to reserve runtime controller port")?;
    Ok((proxy, controller))
}

fn error_chain_reports_bind_conflict(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let text = cause.to_string().to_ascii_lowercase();
        text.contains("address already in use")
            || text.contains("only one usage of each socket address")
            || text.contains("bind:")
            || text.contains("bind failed")
    })
}

fn create_private_temp_dir(id: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("sing-box-tui-node-runtime-{id}"));
    fs::create_dir(&path).with_context(|| format!("failed to create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o700)) {
            let _ = fs::remove_dir(&path);
            return Err(error).with_context(|| format!("failed to protect {}", path.display()));
        }
    }
    Ok(path)
}

fn bounded_log_tail(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(1_024);
    Some(String::from_utf8_lossy(&bytes[start..]).replace(['\r', '\n'], " "))
}

fn random_id() -> String {
    format!("{:016x}", rand::rng().random::<u64>())
}

fn discover_active_environment() -> ManagerResult<(PathBuf, PathBuf)> {
    let directory = active_environment_dir().map_err(|error| {
        ManagerError::new(
            "invalid_environment",
            format!("failed to prepare active environment directory: {error:#}"),
        )
    })?;
    let mut active = Vec::new();
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ManagerError::new(
                "no_active_runtime_environment",
                "no verified active sing-box-tui environment was found; provide config_path and sing_box_executable",
            ));
        }
        Err(error) => {
            return Err(ManagerError::new(
                "invalid_environment",
                format!("failed to inspect active environments: {error}"),
            ));
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<ActiveEnvironment>(&text).ok());
        let Some(metadata) = metadata else {
            let _ = fs::remove_file(path);
            continue;
        };
        if !process_is_alive(metadata.sing_box_pid)
            || !managed_sing_box_process_matches(
                metadata.sing_box_pid,
                &metadata.sing_box_executable,
                &metadata.config_path,
            )
        {
            let _ = fs::remove_file(path);
            continue;
        }
        active.push(metadata);
    }
    match active.len() {
        0 => Err(ManagerError::new(
            "no_active_runtime_environment",
            "no verified active sing-box-tui environment was found; provide config_path and sing_box_executable",
        )),
        1 => {
            let environment = active.remove(0);
            Ok((environment.config_path, environment.sing_box_executable))
        }
        _ => Err(ManagerError::new(
            "ambiguous_runtime_environment",
            format!(
                "multiple active sing-box-tui environments exist: {}",
                active
                    .iter()
                    .map(|item| format!(
                        "pid={} config={}",
                        item.sing_box_pid,
                        item.config_path.display()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

#[derive(serde::Deserialize, Serialize)]
struct ActiveEnvironment {
    owner_pid: u32,
    sing_box_pid: u32,
    config_path: PathBuf,
    sing_box_executable: PathBuf,
}

pub(crate) fn register_active_environment(
    sing_box_pid: u32,
    config_path: &Path,
    sing_box_executable: &Path,
) -> Result<()> {
    let directory = active_environment_dir()?;
    let metadata = ActiveEnvironment {
        owner_pid: std::process::id(),
        sing_box_pid,
        config_path: config_path.canonicalize().with_context(|| {
            format!("failed to resolve active config {}", config_path.display())
        })?,
        sing_box_executable: resolve_sing_box_executable(sing_box_executable)?,
    };
    let path = directory.join(format!("{}.json", metadata.sing_box_pid));
    let encoded = serde_json::to_vec(&metadata).context("failed to encode active environment")?;
    fs::write(&path, encoded).with_context(|| format!("failed to write {}", path.display()))
}

pub(crate) fn unregister_owned_active_environments() -> Result<()> {
    let directory = active_environment_dir()?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("failed to inspect active environments"),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let owned = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<ActiveEnvironment>(&text).ok())
            .is_some_and(|metadata| metadata.owner_pid == std::process::id());
        if owned {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn active_environment_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "sing-box-tui-user-{}",
                // SAFETY: geteuid has no preconditions and does not mutate memory.
                unsafe { libc::geteuid() }
            ))
        });
    #[cfg(not(unix))]
    let base = std::env::temp_dir();

    let directory = base.join(ACTIVE_ENVIRONMENT_DIR);
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    #[cfg(unix)]
    protect_unix_user_directory(&directory)?;
    Ok(directory)
}

#[cfg(unix)]
fn protect_unix_user_directory(directory: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {}", directory.display()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!("active environment path is not a real directory");
    }
    // SAFETY: geteuid has no preconditions and does not mutate memory.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        bail!("active environment directory is owned by another user");
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to protect {}", directory.display()))?;
    let protected = fs::metadata(directory)?;
    if protected.mode() & 0o077 != 0 {
        bail!("active environment directory permissions are not private");
    }
    Ok(())
}

type ManagerResult<T> = std::result::Result<T, ManagerError>;

struct ManagerError {
    code: &'static str,
    message: String,
}

impl ManagerError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn object_params(value: &Value) -> ManagerResult<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| ManagerError::new("invalid_params", "params must be a JSON object"))
}

fn required_string<'a>(params: &'a Map<String, Value>, name: &str) -> ManagerResult<&'a str> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ManagerError::new("invalid_params", format!("{name} must be a string")))
}

fn optional_path(params: &Map<String, Value>, name: &str) -> ManagerResult<Option<PathBuf>> {
    params
        .get(name)
        .map(|value| {
            value.as_str().map(PathBuf::from).ok_or_else(|| {
                ManagerError::new("invalid_params", format!("{name} must be a string"))
            })
        })
        .transpose()
}

fn optional_u64(params: &Map<String, Value>, name: &str) -> ManagerResult<Option<u64>> {
    params
        .get(name)
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                ManagerError::new(
                    "invalid_params",
                    format!("{name} must be an unsigned integer"),
                )
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::{NodeRuntimeManager, RpcRequest, enumerate_nodes, reserve_runtime_ports};

    #[test]
    fn runtime_ports_are_reserved_together_and_cannot_collide() {
        let (proxy, controller) = reserve_runtime_ports().expect("ports can be reserved");
        let proxy_address = proxy.local_addr().unwrap();
        let controller_address = controller.local_addr().unwrap();

        assert_ne!(proxy_address.port(), controller_address.port());
        assert!(std::net::TcpListener::bind(proxy_address).is_err());
        assert!(std::net::TcpListener::bind(controller_address).is_err());
    }

    #[test]
    fn all_selectors_expand_to_unique_concrete_nodes_in_config_order() {
        let config = json!({
            "outbounds": [
                {"type":"selector","tag":"all","outbounds":["nested","node-a","direct"]},
                {"type":"selector","tag":"nested","outbounds":["node-b","node-a"]},
                {"type":"selector","tag":"other","outbounds":["node-b","node-c","block"]},
                {"type":"vmess","tag":"node-a"},
                {"type":"trojan","tag":"node-b"},
                {"type":"vless","tag":"node-c"},
                {"type":"direct","tag":"direct"},
                {"type":"block","tag":"block"}
            ]
        });

        let nodes = enumerate_nodes(&config).expect("selector graph is valid");
        let actual = nodes
            .iter()
            .map(|node| (node.tag.as_str(), node.selectors.clone()))
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![
                (
                    "node-b",
                    vec!["all".to_string(), "nested".to_string(), "other".to_string()]
                ),
                ("node-a", vec!["all".to_string(), "nested".to_string()]),
                ("node-c", vec!["other".to_string()]),
            ]
        );
    }

    #[test]
    fn initialize_returns_the_immutable_node_snapshot_over_the_rpc_interface() {
        let directory = std::env::temp_dir().join(format!(
            "sing-box-tui-node-runtime-test-{}",
            super::random_id()
        ));
        fs::create_dir(&directory).unwrap();
        let config_path = directory.join("config.json");
        fs::write(
            &config_path,
            r#"{"outbounds":[{"type":"selector","tag":"all","outbounds":["n1"]},{"type":"vmess","tag":"n1"}]}"#,
        )
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let manager = NodeRuntimeManager::default();

        let response = manager.handle(RpcRequest {
            id: json!(41),
            method: "initialize".to_string(),
            params: json!({
                "config_path": config_path,
                "sing_box_executable": executable,
                "max_runtimes": 2
            }),
        });

        assert!(response.error.is_none());
        assert_eq!(response.id, json!(41));
        assert_eq!(response.result.unwrap()["total_candidates"], json!(1));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn derived_runtime_config_replaces_live_network_side_effects() {
        let source = json!({
            "inbounds":[{"type":"tun","tag":"live-tun","auto_route":true}],
            "outbounds":[{"type":"vmess","tag":"n1"}],
            "route":{"rules":[{"action":"reject"}],"final":"n1"},
            "experimental":{"clash_api":{"external_controller":"127.0.0.1:9999"}}
        });
        let nodes = vec![super::NodeDescriptor {
            tag: "n1".to_string(),
            selectors: vec!["all".to_string()],
        }];

        let derived = super::build_runtime_config(
            source,
            &nodes,
            31001,
            31002,
            "runtime-selector",
            "runtime-block",
        )
        .unwrap();

        assert_eq!(derived["inbounds"][0]["type"], "mixed");
        assert_eq!(derived["inbounds"][0]["listen"], "127.0.0.1");
        assert_eq!(derived["route"], json!({"final":"runtime-selector"}));
        assert_eq!(
            derived["experimental"]["clash_api"]["external_controller"],
            "127.0.0.1:31002"
        );
        assert_eq!(derived["outbounds"].as_array().unwrap().len(), 3);
    }
}
