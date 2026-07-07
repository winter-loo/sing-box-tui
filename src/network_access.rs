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
pub(crate) struct RemoteAccessProviderManifest {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) protocol: String,
    pub(crate) executable: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) capabilities: RemoteAccessProviderCapabilities,
    #[serde(default)]
    pub(crate) config_schema: Value,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RemoteAccessProviderCapabilities {
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
pub(crate) enum RemoteAccessCommand {
    Connect {
        id: String,
        provider: String,
        config: Value,
    },
    Disconnect {
        id: String,
        provider: String,
        session_id: Option<String>,
    },
    Status {
        id: String,
        provider: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteAccessState {
    Disabled,
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Error,
}

impl RemoteAccessState {
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
pub(crate) struct RemoteAccessRoute {
    pub(crate) cidr: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RemoteAccessBridge {
    pub(crate) kind: String,
    pub(crate) listen: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum RemoteAccessEvent {
    StateChanged {
        provider: String,
        state: RemoteAccessState,
        message: String,
    },
    RoutesPushed {
        provider: String,
        session_id: Option<String>,
        routes: Vec<RemoteAccessRoute>,
        dns: Vec<String>,
        bridge: Option<RemoteAccessBridge>,
    },
    Error {
        provider: String,
        code: String,
        message: String,
    },
    Log {
        provider: String,
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RemoteAccessEventEnvelope {
    #[serde(rename = "type")]
    pub(crate) message_type: String,
    #[serde(flatten)]
    pub(crate) event: RemoteAccessEvent,
}

impl RemoteAccessEventEnvelope {
    pub(crate) fn new(event: RemoteAccessEvent) -> Self {
        Self {
            message_type: "event".to_string(),
            event,
        }
    }
}

pub(crate) struct ExternalRemoteAccessProvider {
    manifest: RemoteAccessProviderManifest,
    child: Child,
    stdin: ChildStdin,
    event_rx: Receiver<Result<RemoteAccessEventEnvelope, String>>,
    stdout_worker: Option<JoinHandle<()>>,
    stderr_worker: Option<JoinHandle<()>>,
}

impl ExternalRemoteAccessProvider {
    pub(crate) fn spawn(manifest: RemoteAccessProviderManifest) -> Result<Self> {
        validate_remote_access_manifest(&manifest)?;
        let executable = resolve_manifest_executable(&manifest)?;
        let mut command = Command::new(executable);
        command.args(&manifest.args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn remote access provider {}", manifest.id))?;
        let stdin = child.stdin.take().context("provider stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("provider stdout was not piped")?;
        let stderr = child
            .stderr
            .take()
            .context("provider stderr was not piped")?;
        let (tx, rx) = mpsc::channel();
        let provider_id = manifest.id.clone();
        let stdout_tx = tx.clone();
        let stdout_worker = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        let event = serde_json::from_str::<RemoteAccessEventEnvelope>(&line)
                            .map_err(|error| {
                                format!("failed to parse provider event JSON: {error}; line={line}")
                            });
                        let _ = stdout_tx.send(event);
                    }
                    Err(error) => {
                        let _ = stdout_tx.send(Err(format!(
                            "failed to read provider stdout for {provider_id}: {error}"
                        )));
                        break;
                    }
                }
            }
        });
        let stderr_provider_id = manifest.id.clone();
        let stderr_tx = tx.clone();
        let stderr_worker = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        // Provider diagnostics used to be mirrored with eprintln!, which broke
                        // the TUI alternate screen when a protocol session became chatty. Keep
                        // stderr useful by translating it into regular provider log events.
                        let event = RemoteAccessEventEnvelope::new(RemoteAccessEvent::Log {
                            provider: stderr_provider_id.clone(),
                            message: line,
                        });
                        let _ = stderr_tx.send(Ok(event));
                    }
                    Err(error) => {
                        let _ = stderr_tx.send(Err(format!(
                            "failed to read provider stderr for {stderr_provider_id}: {error}"
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

    pub(crate) fn provider_id(&self) -> &str {
        &self.manifest.id
    }

    pub(crate) fn send(&mut self, command: &RemoteAccessCommand) -> Result<()> {
        let line =
            serde_json::to_string(command).context("failed to encode remote access command")?;
        writeln!(self.stdin, "{line}").context("failed to write remote access command")?;
        self.stdin
            .flush()
            .context("failed to flush remote access command")?;
        Ok(())
    }

    pub(crate) fn try_recv(&self) -> Result<Option<RemoteAccessEventEnvelope>, String> {
        match self.event_rx.try_recv() {
            Ok(Ok(event)) => Ok(Some(event)),
            Ok(Err(error)) => Err(error),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("provider event stream closed".to_string()),
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

pub(crate) fn load_remote_access_manifest(path: &Path) -> Result<RemoteAccessProviderManifest> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read remote access manifest {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse remote access manifest {}", path.display()))
}

pub(crate) fn default_hillstone_manifest() -> Result<RemoteAccessProviderManifest> {
    let exe = std::env::current_exe().context("failed to locate current executable")?;
    Ok(RemoteAccessProviderManifest {
        id: "hillstone".to_string(),
        name: "Hillstone Secure Connect".to_string(),
        kind: "remote_access".to_string(),
        protocol: "hillstone-secure-connect".to_string(),
        executable: exe.to_string_lossy().to_string(),
        args: vec![
            "remote-access-provider".to_string(),
            "hillstone".to_string(),
            "--stdio".to_string(),
        ],
        version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: RemoteAccessProviderCapabilities {
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
struct HillstoneProviderConfig {
    #[serde(default = "default_hillstone_provider_mode")]
    mode: HillstoneProviderMode,
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
enum HillstoneProviderMode {
    Bridge,
    Tun,
}

fn default_hillstone_provider_mode() -> HillstoneProviderMode {
    HillstoneProviderMode::Bridge
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

pub(crate) fn run_remote_access_provider_stdio(provider: &str, stdio: bool) -> Result<()> {
    if !stdio {
        bail!("remote-access-provider currently requires --stdio");
    }
    match provider {
        "hillstone" => run_hillstone_remote_access_provider_stdio(),
        value => bail!("unsupported remote access provider: {value}"),
    }
}

fn run_hillstone_remote_access_provider_stdio() -> Result<()> {
    let sink = Arc::new(JsonLineEventSink::new("hillstone", io::stdout()));
    let mut session: Option<RemoteAccessProviderSession> = None;
    for line in io::stdin().lock().lines() {
        let line = line.context("failed to read remote access provider command")?;
        if line.trim().is_empty() {
            continue;
        }
        let command: RemoteAccessCommand =
            serde_json::from_str(&line).context("failed to parse remote access command JSON")?;
        match command {
            RemoteAccessCommand::Connect {
                provider, config, ..
            } => {
                if provider != "hillstone" {
                    emit_provider_error(&sink, "invalid_provider", "command provider mismatch")?;
                    continue;
                }
                if session.is_some() {
                    emit_provider_error(
                        &sink,
                        "already_connected",
                        "provider session already exists",
                    )?;
                    continue;
                }
                let config: HillstoneProviderConfig = serde_json::from_value(config)
                    .context("failed to parse Hillstone remote access config")?;
                session = Some(start_hillstone_provider_session(config, Arc::clone(&sink))?);
            }
            RemoteAccessCommand::Disconnect { .. } => {
                if let Some(session) = session.take() {
                    sink.state(RemoteAccessState::Disconnecting, "disconnect requested")?;
                    session.shutdown.store(true, Ordering::SeqCst);
                    let _ = session.worker.join();
                } else {
                    sink.state(RemoteAccessState::Disconnected, "no active session")?;
                }
            }
            RemoteAccessCommand::Status { .. } => {
                let state = if session.is_some() {
                    RemoteAccessState::Connected
                } else {
                    RemoteAccessState::Disconnected
                };
                sink.state(state, "status requested")?;
            }
        }
    }
    if let Some(session) = session.take() {
        session.shutdown.store(true, Ordering::SeqCst);
        let _ = session.worker.join();
    }
    Ok(())
}

struct RemoteAccessProviderSession {
    shutdown: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

fn start_hillstone_provider_session(
    config: HillstoneProviderConfig,
    sink: Arc<JsonLineEventSink<io::Stdout>>,
) -> Result<RemoteAccessProviderSession> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_sink = Arc::clone(&sink);
    sink.state(
        RemoteAccessState::Connecting,
        "starting Hillstone provider session",
    )?;
    let worker = thread::spawn(move || {
        let result = match config.mode {
            HillstoneProviderMode::Bridge => run_hillstone_probe(HillstoneProbeOptions {
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
                // In provider mode the child process only reports pushed routes. The TUI applies
                // provider-owned sing-box rules so route ownership, errors, and reload prompts stay
                // in the single user-facing control surface.
                apply_routes_for_proxy: false,
                route_proxy: None,
                event_sink: Some(worker_sink.clone()),
                shutdown: Some(worker_shutdown),
            }),
            HillstoneProviderMode::Tun => run_hillstone_probe(HillstoneProbeOptions {
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
            let _ = worker_sink.state(RemoteAccessState::Error, "provider session failed");
        }
    });
    Ok(RemoteAccessProviderSession { shutdown, worker })
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

fn emit_provider_error(
    sink: &JsonLineEventSink<io::Stdout>,
    code: &str,
    message: &str,
) -> Result<()> {
    sink.error(code, message)?;
    sink.state(RemoteAccessState::Error, message)
}

struct JsonLineEventSink<W: Write + Send + 'static> {
    provider: String,
    writer: Mutex<W>,
}

impl<W: Write + Send + 'static> JsonLineEventSink<W> {
    fn new(provider: &str, writer: W) -> Self {
        Self {
            provider: provider.to_string(),
            writer: Mutex::new(writer),
        }
    }

    fn emit(&self, event: RemoteAccessEvent) -> Result<()> {
        let envelope = RemoteAccessEventEnvelope::new(event);
        let line =
            serde_json::to_string(&envelope).context("failed to encode remote access event")?;
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("remote access event writer mutex poisoned"))?;
        writeln!(writer, "{line}").context("failed to write remote access event")?;
        writer
            .flush()
            .context("failed to flush remote access event")?;
        Ok(())
    }

    fn state(&self, state: RemoteAccessState, message: &str) -> Result<()> {
        self.emit(RemoteAccessEvent::StateChanged {
            provider: self.provider.clone(),
            state,
            message: message.to_string(),
        })
    }

    fn error(&self, code: &str, message: &str) -> Result<()> {
        self.emit(RemoteAccessEvent::Error {
            provider: self.provider.clone(),
            code: code.to_string(),
            message: message.to_string(),
        })
    }
}

impl<W: Write + Send + 'static> HillstoneEventSink for JsonLineEventSink<W> {
    fn state_changed(&self, state: &str, message: &str) -> Result<()> {
        let state = match state {
            "connecting" => RemoteAccessState::Connecting,
            "connected" => RemoteAccessState::Connected,
            "disconnecting" => RemoteAccessState::Disconnecting,
            "disconnected" => RemoteAccessState::Disconnected,
            "error" => RemoteAccessState::Error,
            _ => RemoteAccessState::Error,
        };
        self.state(state, message)
    }

    fn routes_pushed(&self, info: &HillstoneNetworkInfo) -> Result<()> {
        self.emit(RemoteAccessEvent::RoutesPushed {
            provider: self.provider.clone(),
            session_id: None,
            routes: info
                .route_cidrs
                .iter()
                .cloned()
                .map(|cidr| RemoteAccessRoute { cidr })
                .collect(),
            dns: info.dns_ipv4.iter().map(ToString::to_string).collect(),
            bridge: info.bridge_listen.map(|listen| RemoteAccessBridge {
                kind: "http".to_string(),
                listen: listen.to_string(),
            }),
        })
    }
}

fn validate_remote_access_manifest(manifest: &RemoteAccessProviderManifest) -> Result<()> {
    if manifest.id.trim().is_empty() {
        bail!("remote access provider manifest id cannot be empty");
    }
    if manifest.kind != "remote_access" {
        bail!(
            "remote access provider {} has unsupported kind {}",
            manifest.id,
            manifest.kind
        );
    }
    if manifest.executable.trim().is_empty() {
        bail!(
            "remote access provider {} executable cannot be empty",
            manifest.id
        );
    }
    Ok(())
}

fn resolve_manifest_executable(manifest: &RemoteAccessProviderManifest) -> Result<PathBuf> {
    let path = PathBuf::from(&manifest.executable);
    if path.is_absolute() {
        return Ok(path);
    }
    // The first implementation can use either an absolute executable from the built-in
    // manifest or a manifest-local relative path. Resolving relative paths up front keeps
    // process spawning deterministic and avoids depending on whatever CWD the TUI has.
    Ok(std::env::current_dir()
        .context("failed to resolve current directory for provider executable")?
        .join(path))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        ExternalRemoteAccessProvider, HillstoneProviderConfig, HillstoneProviderMode,
        RemoteAccessCommand, RemoteAccessEvent, RemoteAccessEventEnvelope,
        RemoteAccessProviderCapabilities, RemoteAccessProviderManifest, RemoteAccessRoute,
        RemoteAccessState, default_hillstone_manifest, normalize_tun_helper_command,
    };
    use serde_json::json;

    #[test]
    fn remote_access_command_serializes_as_json_line_protocol() {
        let command = RemoteAccessCommand::Connect {
            id: "cmd-1".to_string(),
            provider: "hillstone".to_string(),
            config: json!({
                "server": "sslvpn.example.com",
                "username": "user",
            }),
        };

        let value = serde_json::to_value(command).expect("command serializes");
        assert_eq!(value["type"], "connect");
        assert_eq!(value["provider"], "hillstone");
        assert_eq!(value["config"]["server"], "sslvpn.example.com");
    }

    #[test]
    fn remote_access_event_matches_rfc_envelope_shape() {
        let event = RemoteAccessEventEnvelope::new(RemoteAccessEvent::RoutesPushed {
            provider: "hillstone".to_string(),
            session_id: Some("local".to_string()),
            routes: vec![RemoteAccessRoute {
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
    fn remote_access_state_labels_are_user_facing() {
        assert_eq!(RemoteAccessState::Connected.label(), "connected");
        assert_eq!(RemoteAccessState::Disconnected.label(), "disconnected");
    }

    #[test]
    fn built_in_hillstone_manifest_uses_remote_access_provider_subcommand() {
        let manifest = default_hillstone_manifest().expect("manifest builds");
        assert_eq!(manifest.id, "hillstone");
        assert_eq!(manifest.kind, "remote_access");
        assert!(
            manifest
                .args
                .iter()
                .any(|arg| arg == "remote-access-provider")
        );
        assert_eq!(manifest.config_schema["password"]["sensitive"], true);
        assert_eq!(manifest.config_schema["mode"]["default"], "bridge");
        assert_eq!(
            manifest.config_schema["bridge_listen"]["default"],
            "127.0.0.1:16780"
        );
    }

    #[test]
    fn hillstone_provider_config_accepts_direct_password() {
        let config: HillstoneProviderConfig = serde_json::from_value(json!({
            "server": "sslvpn.example.com",
            "username": "user",
            "password": "secret"
        }))
        .expect("config parses");

        assert_eq!(config.mode, HillstoneProviderMode::Bridge);
        assert_eq!(config.password.as_deref(), Some("secret"));
        assert_eq!(config.password_env, None);
    }

    #[test]
    fn hillstone_provider_config_accepts_tun_mode() {
        let config: HillstoneProviderConfig = serde_json::from_value(json!({
            "mode": "tun",
            "server": "sslvpn.example.com",
            "username": "user"
        }))
        .expect("config parses");

        assert_eq!(config.mode, HillstoneProviderMode::Tun);
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
                " remote-access-tun-helper ".to_string(),
                " --stdio ".to_string()
            ])),
            Some(vec![
                "sudo".to_string(),
                "sing-box-tui".to_string(),
                "remote-access-tun-helper".to_string(),
                "--stdio".to_string()
            ])
        );
    }

    #[test]
    fn provider_stderr_is_reported_as_log_event() {
        let manifest = RemoteAccessProviderManifest {
            id: "fake".to_string(),
            name: "Fake Provider".to_string(),
            kind: "remote_access".to_string(),
            protocol: "test".to_string(),
            executable: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo provider diagnostic >&2; sleep 0.2".to_string(),
            ],
            version: "0.0.0".to_string(),
            capabilities: RemoteAccessProviderCapabilities::default(),
            config_schema: json!({}),
        };
        let provider = ExternalRemoteAccessProvider::spawn(manifest).expect("fake provider spawns");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_log = false;

        while Instant::now() < deadline {
            match provider.try_recv() {
                Ok(Some(event)) => {
                    if let RemoteAccessEvent::Log { provider, message } = event.event {
                        assert_eq!(provider, "fake");
                        assert_eq!(message, "provider diagnostic");
                        saw_log = true;
                        break;
                    }
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) if error == "provider event stream closed" => break,
                Err(error) => panic!("unexpected provider error: {error}"),
            }
        }

        provider.stop().expect("fake provider stops");
        assert!(saw_log, "provider stderr should become a log event");
    }
}
