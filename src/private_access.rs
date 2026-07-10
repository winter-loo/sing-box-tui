use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::defaults::DEFAULT_CONFIG_PATH;
use crate::hillstone::{
    HillstoneEventSink, HillstoneNetworkInfo, HillstoneProbeOptions, run_hillstone_probe,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PrivateAccessServiceManifest {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) protocol: String,
    pub(crate) executable: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) capabilities: PrivateAccessServiceCapabilities,
    #[serde(default)]
    pub(crate) config_schema: Value,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PrivateAccessServiceCapabilities {
    #[serde(default)]
    pub(crate) pushed_routes: bool,
    #[serde(default)]
    pub(crate) pushed_dns: bool,
    #[serde(default)]
    pub(crate) local_http_bridge: bool,
    #[serde(default)]
    pub(crate) graceful_disconnect: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum PrivateAccessCommand {
    Connect {
        id: String,
        service: String,
        config: Value,
    },
    Disconnect {
        id: String,
        service: String,
        session_id: Option<String>,
    },
    Detach {
        id: String,
        service: String,
        session_id: Option<String>,
    },
    Status {
        id: String,
        service: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrivateAccessState {
    Disabled,
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Error,
}

impl PrivateAccessState {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Disconnecting => "disconnecting",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PrivateAccessRoute {
    pub(crate) cidr: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PrivateAccessBridge {
    pub(crate) kind: String,
    pub(crate) listen: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum PrivateAccessEvent {
    StateChanged {
        service: String,
        state: PrivateAccessState,
        message: String,
    },
    RoutesPushed {
        service: String,
        session_id: Option<String>,
        routes: Vec<PrivateAccessRoute>,
        dns: Vec<String>,
        bridge: Option<PrivateAccessBridge>,
    },
    Error {
        service: String,
        code: String,
        message: String,
    },
    Log {
        service: String,
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PrivateAccessEventEnvelope {
    #[serde(rename = "type")]
    pub(crate) message_type: String,
    #[serde(flatten)]
    pub(crate) event: PrivateAccessEvent,
}

impl PrivateAccessEventEnvelope {
    pub(crate) fn new(event: PrivateAccessEvent) -> Self {
        Self {
            message_type: "event".to_string(),
            event,
        }
    }
}

pub(crate) struct ExternalPrivateAccessService {
    manifest: PrivateAccessServiceManifest,
    child: Child,
    stdin: ChildStdin,
    event_rx: Receiver<Result<PrivateAccessEventEnvelope, String>>,
    stdout_worker: Option<JoinHandle<()>>,
    stderr_worker: Option<JoinHandle<()>>,
}

impl ExternalPrivateAccessService {
    pub(crate) fn spawn(manifest: PrivateAccessServiceManifest) -> Result<Self> {
        validate_private_access_manifest(&manifest)?;
        let executable = resolve_manifest_executable(&manifest)?;
        let mut command = Command::new(executable);
        command.args(&manifest.args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn private access service {}", manifest.id))?;
        let stdin = child.stdin.take().context("service stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("service stdout was not piped")?;
        let stderr = child
            .stderr
            .take()
            .context("service stderr was not piped")?;
        let (tx, rx) = mpsc::channel();
        let service_id = manifest.id.clone();
        let stdout_tx = tx.clone();
        let stdout_worker = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        let event = serde_json::from_str::<PrivateAccessEventEnvelope>(&line)
                            .map_err(|error| {
                                format!("failed to parse service event JSON: {error}; line={line}")
                            });
                        let _ = stdout_tx.send(event);
                    }
                    Err(error) => {
                        let _ = stdout_tx.send(Err(format!(
                            "failed to read service stdout for {service_id}: {error}"
                        )));
                        break;
                    }
                }
            }
        });
        let stderr_service_id = manifest.id.clone();
        let stderr_tx = tx.clone();
        let stderr_worker = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        // Service diagnostics used to be mirrored with eprintln!, which broke
                        // the TUI alternate screen when a protocol session became chatty. Keep
                        // stderr useful by translating it into regular service log events.
                        let event = PrivateAccessEventEnvelope::new(PrivateAccessEvent::Log {
                            service: stderr_service_id.clone(),
                            message: line,
                        });
                        let _ = stderr_tx.send(Ok(event));
                    }
                    Err(error) => {
                        let _ = stderr_tx.send(Err(format!(
                            "failed to read service stderr for {stderr_service_id}: {error}"
                        )));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            manifest,
            child,
            stdin,
            event_rx: rx,
            stdout_worker: Some(stdout_worker),
            stderr_worker: Some(stderr_worker),
        })
    }

    pub(crate) fn service_id(&self) -> &str {
        &self.manifest.id
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn send(&mut self, command: &PrivateAccessCommand) -> Result<()> {
        let line =
            serde_json::to_string(command).context("failed to encode private access command")?;
        writeln!(self.stdin, "{line}").context("failed to write private access command")?;
        self.stdin
            .flush()
            .context("failed to flush private access command")?;
        Ok(())
    }

    pub(crate) fn detach(&mut self) -> Result<()> {
        let service = self.service_id().to_string();
        self.send(&PrivateAccessCommand::Detach {
            id: "tui-background".to_string(),
            service,
            session_id: None,
        })
    }

    pub(crate) fn try_recv(&self) -> Result<Option<PrivateAccessEventEnvelope>, String> {
        match self.event_rx.try_recv() {
            Ok(Ok(event)) => Ok(Some(event)),
            Ok(Err(error)) => Err(error),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("service event stream closed".to_string()),
        }
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(worker) = self.stdout_worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.stderr_worker.take() {
            let _ = worker.join();
        }
        Ok(())
    }
}

pub(crate) fn load_private_access_manifest(path: &Path) -> Result<PrivateAccessServiceManifest> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read private access manifest {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse private access manifest {}", path.display()))
}

pub(crate) fn default_hillstone_manifest() -> Result<PrivateAccessServiceManifest> {
    let exe = std::env::current_exe().context("failed to locate current executable")?;
    Ok(PrivateAccessServiceManifest {
        id: "hillstone".to_string(),
        name: "Hillstone Secure Connect".to_string(),
        kind: "private_access".to_string(),
        protocol: "hillstone-secure-connect".to_string(),
        executable: exe.to_string_lossy().to_string(),
        args: vec![
            "private-access-service".to_string(),
            "hillstone".to_string(),
            "--stdio".to_string(),
        ],
        version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: PrivateAccessServiceCapabilities {
            pushed_routes: true,
            pushed_dns: true,
            local_http_bridge: true,
            graceful_disconnect: true,
        },
        config_schema: json!({
            "mode": { "type": "string", "enum": ["bridge", "tun"], "default": "bridge" },
            "server": { "type": "string", "required": true },
            "port": { "type": "integer", "default": 4433 },
            "username": { "type": "string", "required": true },
            "password": { "type": "string", "required": false, "sensitive": true },
            "password_env": { "type": "string", "required": false },
            "bridge_listen": { "type": "string", "default": "127.0.0.1:16780" },
            "tun_helper": { "type": "array", "items": { "type": "string" }, "required": false },
            "tls_verify": { "type": "boolean", "default": true }
        }),
    })
}

#[derive(Clone, Debug, Deserialize)]
struct HillstoneServiceConfig {
    #[serde(default = "default_hillstone_service_mode")]
    mode: HillstoneServiceMode,
    server: String,
    #[serde(default = "default_hillstone_port")]
    port: u16,
    username: String,
    #[serde(default)]
    password: Option<String>,
    password_env: Option<String>,
    #[serde(default = "default_hillstone_bridge_listen")]
    bridge_listen: String,
    #[serde(default)]
    tun_helper: Option<Vec<String>>,
    #[serde(default)]
    tls_verify: bool,
    host_id: Option<String>,
    host_name: Option<String>,
    #[serde(default = "default_hillstone_client_version")]
    client_version: String,
    #[serde(default = "default_hillstone_timeout_secs")]
    timeout_secs: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HillstoneServiceMode {
    Bridge,
    Tun,
}

fn default_hillstone_service_mode() -> HillstoneServiceMode {
    HillstoneServiceMode::Bridge
}

fn default_hillstone_port() -> u16 {
    4433
}

fn default_hillstone_bridge_listen() -> String {
    "127.0.0.1:16780".to_string()
}

fn default_hillstone_client_version() -> String {
    "5.7.1.12488".to_string()
}

fn default_hillstone_timeout_secs() -> u64 {
    10
}

pub(crate) fn run_private_access_service_stdio(service: &str, stdio: bool) -> Result<()> {
    if !stdio {
        bail!("private-access-service currently requires --stdio");
    }
    match service {
        "hillstone" => run_hillstone_private_access_service_stdio(),
        value => bail!("unsupported private access service: {value}"),
    }
}

fn run_hillstone_private_access_service_stdio() -> Result<()> {
    let detached = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(JsonLineEventSink::new(
        "hillstone",
        io::stdout(),
        Arc::clone(&detached),
    ));
    let mut session: Option<PrivateAccessServiceSession> = None;
    for line in io::stdin().lock().lines() {
        let line = line.context("failed to read private access service command")?;
        if line.trim().is_empty() {
            continue;
        }
        let command: PrivateAccessCommand =
            serde_json::from_str(&line).context("failed to parse private access command JSON")?;
        match command {
            PrivateAccessCommand::Connect {
                service, config, ..
            } => {
                if service != "hillstone" {
                    emit_service_error(&sink, "invalid_service", "command service mismatch")?;
                    continue;
                }
                if session.is_some() {
                    emit_service_error(
                        &sink,
                        "already_connected",
                        "service session already exists",
                    )?;
                    continue;
                }
                let config: HillstoneServiceConfig = serde_json::from_value(config)
                    .context("failed to parse Hillstone private access config")?;
                session = Some(start_hillstone_service_session(config, Arc::clone(&sink))?);
            }
            PrivateAccessCommand::Disconnect { .. } => {
                if let Some(session) = session.take() {
                    sink.state(PrivateAccessState::Disconnecting, "disconnect requested")?;
                    session.shutdown.store(true, Ordering::SeqCst);
                    let _ = session.worker.join();
                } else {
                    sink.state(PrivateAccessState::Disconnected, "no active session")?;
                }
            }
            PrivateAccessCommand::Detach { service, .. } => {
                if service != "hillstone" {
                    emit_service_error(&sink, "invalid_service", "command service mismatch")?;
                    continue;
                }
                detached.store(true, Ordering::SeqCst);
                let state = if session.is_some() {
                    PrivateAccessState::Connected
                } else {
                    PrivateAccessState::Disconnected
                };
                sink.state(state, "service detached from TUI")?;
            }
            PrivateAccessCommand::Status { .. } => {
                let state = if session.is_some() {
                    PrivateAccessState::Connected
                } else {
                    PrivateAccessState::Disconnected
                };
                sink.state(state, "status requested")?;
            }
        }
    }
    if let Some(session) = session.take() {
        if !detached.load(Ordering::SeqCst) {
            session.shutdown.store(true, Ordering::SeqCst);
        }
        let _ = session.worker.join();
    }
    Ok(())
}

struct PrivateAccessServiceSession {
    shutdown: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

fn start_hillstone_service_session(
    config: HillstoneServiceConfig,
    sink: Arc<JsonLineEventSink<io::Stdout>>,
) -> Result<PrivateAccessServiceSession> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_sink = Arc::clone(&sink);
    sink.state(
        PrivateAccessState::Connecting,
        "starting Hillstone service session",
    )?;
    let worker = thread::spawn(move || {
        let result = match config.mode {
            HillstoneServiceMode::Bridge => run_hillstone_probe(HillstoneProbeOptions {
                server: config.server,
                port: config.port,
                username: config.username,
                password: config.password,
                password_env: config.password_env,
                password_stdin: false,
                host_id: config.host_id,
                host_name: config.host_name,
                client_version: config.client_version,
                timeout_secs: config.timeout_secs,
                verify_server_cert: config.tls_verify,
                stop_before_new_key: false,
                udp_icmp_probe: false,
                udp_tcp_probe: None,
                udp_http_get: None,
                udp_http_proxy: Some(config.bridge_listen),
                tun_data_plane: false,
                tun_helper_command: None,
                route_config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
                apply_routes: false,
                // In service mode the child process only reports pushed routes. The TUI applies
                // service-owned sing-box rules so route ownership, errors, and reload prompts stay
                // in the single user-facing control surface.
                apply_routes_for_proxy: false,
                route_proxy: None,
                event_sink: Some(worker_sink.clone()),
                shutdown: Some(worker_shutdown),
            }),
            HillstoneServiceMode::Tun => run_hillstone_probe(HillstoneProbeOptions {
                server: config.server,
                port: config.port,
                username: config.username,
                password: config.password,
                password_env: config.password_env,
                password_stdin: false,
                host_id: config.host_id,
                host_name: config.host_name,
                client_version: config.client_version,
                timeout_secs: config.timeout_secs,
                verify_server_cert: config.tls_verify,
                stop_before_new_key: false,
                udp_icmp_probe: false,
                udp_tcp_probe: None,
                udp_http_get: None,
                udp_http_proxy: None,
                tun_data_plane: true,
                tun_helper_command: normalize_tun_helper_command(config.tun_helper),
                route_config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
                apply_routes: false,
                apply_routes_for_proxy: false,
                route_proxy: None,
                event_sink: Some(worker_sink.clone()),
                shutdown: Some(worker_shutdown),
            }),
        };
        if let Err(error) = result {
            let _ = worker_sink.error("session_failed", &format!("{error:#}"));
            let _ = worker_sink.state(PrivateAccessState::Error, "service session failed");
        }
    });
    Ok(PrivateAccessServiceSession { shutdown, worker })
}

fn normalize_tun_helper_command(command: Option<Vec<String>>) -> Option<Vec<String>> {
    command.and_then(|command| {
        let command = command
            .into_iter()
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        if command.is_empty() {
            None
        } else {
            Some(command)
        }
    })
}

fn emit_service_error(
    sink: &JsonLineEventSink<io::Stdout>,
    code: &str,
    message: &str,
) -> Result<()> {
    sink.error(code, message)?;
    sink.state(PrivateAccessState::Error, message)
}

struct JsonLineEventSink<W: Write + Send + 'static> {
    service: String,
    writer: Mutex<W>,
    detached: Arc<AtomicBool>,
}

impl<W: Write + Send + 'static> JsonLineEventSink<W> {
    fn new(service: &str, writer: W, detached: Arc<AtomicBool>) -> Self {
        Self {
            service: service.to_string(),
            writer: Mutex::new(writer),
            detached,
        }
    }

    fn emit(&self, event: PrivateAccessEvent) -> Result<()> {
        let envelope = PrivateAccessEventEnvelope::new(event);
        let line =
            serde_json::to_string(&envelope).context("failed to encode private access event")?;
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("private access event writer mutex poisoned"))?;
        if let Err(error) = writeln!(writer, "{line}") {
            if self.detached.load(Ordering::SeqCst) && error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error).context("failed to write private access event");
        }
        if let Err(error) = writer.flush() {
            if self.detached.load(Ordering::SeqCst) && error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error).context("failed to flush private access event");
        }
        Ok(())
    }

    fn state(&self, state: PrivateAccessState, message: &str) -> Result<()> {
        self.emit(PrivateAccessEvent::StateChanged {
            service: self.service.clone(),
            state,
            message: message.to_string(),
        })
    }

    fn error(&self, code: &str, message: &str) -> Result<()> {
        self.emit(PrivateAccessEvent::Error {
            service: self.service.clone(),
            code: code.to_string(),
            message: message.to_string(),
        })
    }
}

impl<W: Write + Send + 'static> HillstoneEventSink for JsonLineEventSink<W> {
    fn state_changed(&self, state: &str, message: &str) -> Result<()> {
        let state = match state {
            "connecting" => PrivateAccessState::Connecting,
            "connected" => PrivateAccessState::Connected,
            "disconnecting" => PrivateAccessState::Disconnecting,
            "disconnected" => PrivateAccessState::Disconnected,
            "error" => PrivateAccessState::Error,
            _ => PrivateAccessState::Error,
        };
        self.state(state, message)
    }

    fn routes_pushed(&self, info: &HillstoneNetworkInfo) -> Result<()> {
        self.emit(PrivateAccessEvent::RoutesPushed {
            service: self.service.clone(),
            session_id: None,
            routes: info
                .route_cidrs
                .iter()
                .cloned()
                .map(|cidr| PrivateAccessRoute { cidr })
                .collect(),
            dns: info.dns_ipv4.iter().map(ToString::to_string).collect(),
            bridge: info.bridge_listen.map(|listen| PrivateAccessBridge {
                kind: "http".to_string(),
                listen: listen.to_string(),
            }),
        })
    }
}

fn validate_private_access_manifest(manifest: &PrivateAccessServiceManifest) -> Result<()> {
    if manifest.id.trim().is_empty() {
        bail!("private access service manifest id cannot be empty");
    }
    if manifest.kind != "private_access" {
        bail!(
            "private access service {} has unsupported kind {}",
            manifest.id,
            manifest.kind
        );
    }
    if manifest.executable.trim().is_empty() {
        bail!(
            "private access service {} executable cannot be empty",
            manifest.id
        );
    }
    Ok(())
}

fn resolve_manifest_executable(manifest: &PrivateAccessServiceManifest) -> Result<PathBuf> {
    let path = PathBuf::from(&manifest.executable);
    if path.is_absolute() {
        return Ok(path);
    }
    // The first implementation can use either an absolute executable from the built-in
    // manifest or a manifest-local relative path. Resolving relative paths up front keeps
    // process spawning deterministic and avoids depending on whatever CWD the TUI has.
    Ok(std::env::current_dir()
        .context("failed to resolve current directory for service executable")?
        .join(path))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{Duration, Instant};

    use super::{
        ExternalPrivateAccessService, HillstoneServiceConfig, HillstoneServiceMode,
        JsonLineEventSink, PrivateAccessCommand, PrivateAccessEvent, PrivateAccessEventEnvelope,
        PrivateAccessRoute, PrivateAccessServiceCapabilities, PrivateAccessServiceManifest,
        PrivateAccessState, default_hillstone_manifest, normalize_tun_helper_command,
    };
    use serde_json::json;

    #[test]
    fn private_access_command_serializes_as_json_line_protocol() {
        let command = PrivateAccessCommand::Connect {
            id: "cmd-1".to_string(),
            service: "hillstone".to_string(),
            config: json!({
                "server": "sslvpn.example.com",
                "username": "user",
            }),
        };

        let value = serde_json::to_value(command).expect("command serializes");
        assert_eq!(value["type"], "connect");
        assert_eq!(value["service"], "hillstone");
        assert_eq!(value["config"]["server"], "sslvpn.example.com");
    }

    #[test]
    fn private_access_detach_command_serializes_as_json_line_protocol() {
        let command = PrivateAccessCommand::Detach {
            id: "cmd-2".to_string(),
            service: "hillstone".to_string(),
            session_id: None,
        };

        let value = serde_json::to_value(command).expect("command serializes");
        assert_eq!(value["type"], "detach");
        assert_eq!(value["service"], "hillstone");
        assert!(value["session_id"].is_null());
    }

    #[test]
    fn private_access_event_matches_rfc_envelope_shape() {
        let event = PrivateAccessEventEnvelope::new(PrivateAccessEvent::RoutesPushed {
            service: "hillstone".to_string(),
            session_id: Some("local".to_string()),
            routes: vec![PrivateAccessRoute {
                cidr: "10.1.0.0/16".to_string(),
            }],
            dns: vec!["10.1.252.10".to_string()],
            bridge: None,
        });

        let value = serde_json::to_value(event).expect("event serializes");
        assert_eq!(value["type"], "event");
        assert_eq!(value["event"], "routes_pushed");
        assert_eq!(value["routes"][0]["cidr"], "10.1.0.0/16");
    }

    #[test]
    fn private_access_state_labels_are_user_facing() {
        assert_eq!(PrivateAccessState::Connected.label(), "connected");
        assert_eq!(PrivateAccessState::Disconnected.label(), "disconnected");
    }

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn detached_service_event_sink_ignores_broken_pipe() {
        let detached = Arc::new(AtomicBool::new(true));
        let sink = JsonLineEventSink::new("hillstone", BrokenPipeWriter, detached);

        sink.state(PrivateAccessState::Connected, "still connected")
            .expect("detached broken pipe should be ignored");
    }

    #[test]
    fn attached_service_event_sink_reports_broken_pipe() {
        let detached = Arc::new(AtomicBool::new(false));
        let sink = JsonLineEventSink::new("hillstone", BrokenPipeWriter, detached);

        let error = sink
            .state(PrivateAccessState::Connected, "still connected")
            .expect_err("attached broken pipe should fail");
        assert!(
            format!("{error:#}").contains("failed to write private access event"),
            "{error:#}"
        );
    }

    #[test]
    fn built_in_hillstone_manifest_uses_private_access_service_subcommand() {
        let manifest = default_hillstone_manifest().expect("manifest builds");
        assert_eq!(manifest.id, "hillstone");
        assert_eq!(manifest.kind, "private_access");
        assert!(
            manifest
                .args
                .iter()
                .any(|arg| arg == "private-access-service")
        );
        assert_eq!(manifest.config_schema["password"]["sensitive"], true);
        assert_eq!(manifest.config_schema["mode"]["default"], "bridge");
        assert_eq!(
            manifest.config_schema["bridge_listen"]["default"],
            "127.0.0.1:16780"
        );
    }

    #[test]
    fn hillstone_service_config_accepts_direct_password() {
        let config: HillstoneServiceConfig = serde_json::from_value(json!({
            "server": "sslvpn.example.com",
            "username": "user",
            "password": "secret"
        }))
        .expect("config parses");

        assert_eq!(config.mode, HillstoneServiceMode::Bridge);
        assert_eq!(config.password.as_deref(), Some("secret"));
        assert_eq!(config.password_env, None);
    }

    #[test]
    fn hillstone_service_config_accepts_tun_mode() {
        let config: HillstoneServiceConfig = serde_json::from_value(json!({
            "mode": "tun",
            "server": "sslvpn.example.com",
            "username": "user"
        }))
        .expect("config parses");

        assert_eq!(config.mode, HillstoneServiceMode::Tun);
    }

    #[test]
    fn empty_tun_helper_command_is_treated_as_default_helper() {
        assert_eq!(normalize_tun_helper_command(None), None);
        assert_eq!(normalize_tun_helper_command(Some(vec![])), None);
        assert_eq!(
            normalize_tun_helper_command(Some(vec![
                " ".to_string(),
                "\t".to_string(),
                "".to_string()
            ])),
            None
        );
        assert_eq!(
            normalize_tun_helper_command(Some(vec![
                " sudo ".to_string(),
                " sing-box-tui ".to_string(),
                " private-access-tun-helper ".to_string(),
                " --stdio ".to_string()
            ])),
            Some(vec![
                "sudo".to_string(),
                "sing-box-tui".to_string(),
                "private-access-tun-helper".to_string(),
                "--stdio".to_string()
            ])
        );
    }

    #[test]
    fn service_stderr_is_reported_as_log_event() {
        #[cfg(windows)]
        let (executable, args) = (
            std::env::var("COMSPEC")
                .unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string()),
            vec![
                "/C".to_string(),
                "echo service diagnostic>&2& ping -n 2 127.0.0.1 >NUL".to_string(),
            ],
        );
        #[cfg(not(windows))]
        let (executable, args) = (
            "/bin/sh".to_string(),
            vec![
                "-c".to_string(),
                "echo service diagnostic >&2; sleep 0.2".to_string(),
            ],
        );
        let manifest = PrivateAccessServiceManifest {
            id: "fake".to_string(),
            name: "Fake Service".to_string(),
            kind: "private_access".to_string(),
            protocol: "test".to_string(),
            executable,
            args,
            version: "0.0.0".to_string(),
            capabilities: PrivateAccessServiceCapabilities::default(),
            config_schema: json!({}),
        };
        let service = ExternalPrivateAccessService::spawn(manifest).expect("fake service spawns");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_log = false;

        while Instant::now() < deadline {
            match service.try_recv() {
                Ok(Some(event)) => {
                    if let PrivateAccessEvent::Log { service, message } = event.event {
                        assert_eq!(service, "fake");
                        assert_eq!(message, "service diagnostic");
                        saw_log = true;
                        break;
                    }
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) if error == "service event stream closed" => break,
                Err(error) => panic!("unexpected service error: {error}"),
            }
        }

        service.stop().expect("fake service stops");
        assert!(saw_log, "service stderr should become a log event");
    }
}
