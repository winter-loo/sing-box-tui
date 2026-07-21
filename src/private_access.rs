use std::collections::HashMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::defaults::DEFAULT_CONFIG_PATH;
use crate::hillstone::{
    HillstoneEventSink, HillstoneNetworkInfo, HillstoneProbeOptions, run_hillstone_probe,
};
use crate::sonicwall::evpn::{
    EstablishedEvpn, EvpnBootstrapOptions, MessageType, NetworkConfig as SonicwallNetworkConfig,
    connect_and_bootstrap as connect_sonicwall_evpn, decode_data_packet as decode_sonicwall_packet,
    encode_data_packet as encode_sonicwall_packet, encode_frame as encode_sonicwall_frame,
};
use crate::sonicwall::{
    SonicwallAuthClient, SonicwallAuthSession, SonicwallAuthStep, SonicwallEvpnIdentity,
    SonicwallLogonCapability, default_agent_info as sonicwall_default_agent_info,
};
use crate::tun::{TunHelperClient, TunHelperStartConfig};

const SONICWALL_DIAGNOSTIC_LOG: &str = "sonicwall-private-access.log";
const HILLSTONE_DIAGNOSTIC_LOG: &str = "hillstone-private-access.log";
const SONICWALL_HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(250);
const SONICWALL_CONTROL_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(45);
const SONICWALL_TUNNEL_DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(60);
const SONICWALL_RAPID_DISCONNECT_WINDOW: Duration = Duration::from_secs(60);
const PRIVATE_ACCESS_EVENT_QUEUE_CAPACITY: usize = 256;
const SONICWALL_RECONNECT_BACKOFFS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(8),
];

#[derive(Debug)]
struct SonicwallReauthenticationRequired {
    reason: String,
}

impl fmt::Display for SonicwallReauthenticationRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SonicWall authentication must be renewed: {}",
            self.reason
        )
    }
}

impl std::error::Error for SonicwallReauthenticationRequired {}

fn sonicwall_reauthentication_required(reason: impl Into<String>) -> anyhow::Error {
    SonicwallReauthenticationRequired {
        reason: reason.into(),
    }
    .into()
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SonicwallTransport {
    #[default]
    Direct,
    Proxy,
}

impl SonicwallTransport {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Proxy => "proxy",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct SonicwallGatewayProfile {
    transport: SonicwallTransport,
    logon_capability: SonicwallLogonCapability,
}

#[derive(Default, Deserialize, Serialize)]
struct SonicwallGatewayProfileCache {
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(default)]
    profiles: HashMap<String, SonicwallGatewayProfile>,
}

impl SonicwallGatewayProfileCache {
    fn with_path(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            profiles: HashMap::new(),
        }
    }

    fn load(path: PathBuf) -> Result<Self> {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::with_path(path));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read SonicWall gateway profile cache {}",
                        path.display()
                    )
                });
            }
        };
        let mut cache: Self = serde_json::from_str(&text).with_context(|| {
            format!(
                "failed to parse SonicWall gateway profile cache {}",
                path.display()
            )
        })?;
        cache.path = Some(path);
        Ok(cache)
    }

    fn persist(&self) -> Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create SonicWall gateway profile cache directory {}",
                    parent.display()
                )
            })?;
        }
        let temp_path = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or_default()
        ));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| {
                format!(
                    "failed to create SonicWall gateway profile cache {}",
                    temp_path.display()
                )
            })?;
        serde_json::to_writer_pretty(&mut file, self)
            .context("failed to encode SonicWall gateway profile cache")?;
        file.write_all(b"\n")
            .context("failed to finish SonicWall gateway profile cache")?;
        file.sync_all()
            .context("failed to flush SonicWall gateway profile cache")?;
        drop(file);
        atomic_replace_sonicwall_gateway_profile_cache(&temp_path, path).with_context(|| {
            format!(
                "failed to replace SonicWall gateway profile cache {}",
                path.display()
            )
        })
    }

    fn get(&self, gateway: &str) -> SonicwallGatewayProfile {
        self.profiles
            .get(&normalize_sonicwall_gateway_cache_key(gateway))
            .copied()
            .unwrap_or_default()
    }

    fn update_transport(&mut self, gateway: &str, transport: SonicwallTransport) {
        self.profiles
            .entry(normalize_sonicwall_gateway_cache_key(gateway))
            .or_default()
            .transport = transport;
    }

    fn update_logon_capability(&mut self, gateway: &str, capability: SonicwallLogonCapability) {
        self.profiles
            .entry(normalize_sonicwall_gateway_cache_key(gateway))
            .or_default()
            .logon_capability = capability;
    }
}

fn sonicwall_gateway_profile_cache_path() -> PathBuf {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("sing-box-tui")
            .join("sonicwall-gateway-profiles.json");
    }
    if let Some(cache_home) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(cache_home)
            .join("sing-box-tui")
            .join("sonicwall-gateway-profiles.json");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("sing-box-tui")
            .join("sonicwall-gateway-profiles.json");
    }
    PathBuf::from("sonicwall-gateway-profiles.json")
}

#[cfg(not(windows))]
fn atomic_replace_sonicwall_gateway_profile_cache(
    temp_path: &Path,
    cache_path: &Path,
) -> Result<()> {
    fs::rename(temp_path, cache_path)?;
    Ok(())
}

#[cfg(windows)]
fn atomic_replace_sonicwall_gateway_profile_cache(
    temp_path: &Path,
    cache_path: &Path,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            lp_existing_file_name: *const u16,
            lp_new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let old_path = temp_path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let new_path = cache_path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            old_path.as_ptr(),
            new_path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn normalize_sonicwall_gateway_cache_key(gateway: &str) -> String {
    gateway.trim().trim_end_matches('/').to_ascii_lowercase()
}

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

#[derive(Deserialize, Serialize)]
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
    AuthReply {
        id: String,
        service: String,
        session_id: String,
        challenge_id: String,
        button: String,
        replies: Vec<PrivateAccessSecret>,
    },
    CancelAuth {
        id: String,
        service: String,
        session_id: String,
        challenge_id: String,
    },
}

#[derive(Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub(crate) struct PrivateAccessSecret(String);

impl PrivateAccessSecret {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PrivateAccessSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PrivateAccessSecret([REDACTED])")
    }
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
pub(crate) struct PrivateAccessAuthOption {
    pub(crate) value: String,
    pub(crate) label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PrivateAccessAuthField {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) kind: String,
    pub(crate) sensitive: bool,
    pub(crate) required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) options: Vec<PrivateAccessAuthOption>,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        domains: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        domain_suffixes: Vec<String>,
        bridge: Option<PrivateAccessBridge>,
    },
    AuthChallenge {
        service: String,
        session_id: String,
        challenge_id: String,
        title: String,
        message: String,
        fields: Vec<PrivateAccessAuthField>,
        buttons: Vec<String>,
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
        let (tx, rx) = mpsc::sync_channel(PRIVATE_ACCESS_EVENT_QUEUE_CAPACITY);
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
                        if stderr_service_id == "hillstone" {
                            append_hillstone_diagnostic("runtime", &line);
                        }
                        // Service diagnostics used to be mirrored with eprintln!, which broke
                        // the TUI alternate screen when a protocol session became chatty. Keep
                        // stderr useful by translating it into regular service log events.
                        let event = PrivateAccessEventEnvelope::new(PrivateAccessEvent::Log {
                            service: stderr_service_id.clone(),
                            message: line,
                        });
                        // Diagnostics are best-effort; never let a chatty stderr producer grow an
                        // unbounded queue or block higher-priority protocol events on stdout.
                        let _ = stderr_tx.try_send(Ok(event));
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

pub(crate) fn default_sonicwall_manifest() -> Result<PrivateAccessServiceManifest> {
    let exe = std::env::current_exe().context("failed to locate current executable")?;
    Ok(PrivateAccessServiceManifest {
        id: "sonicwall".to_string(),
        name: "SonicWall SMA 1000 (clean-room)".to_string(),
        kind: "private_access".to_string(),
        protocol: "sonicwall-sma1000-evpn".to_string(),
        executable: exe.to_string_lossy().to_string(),
        args: vec![
            "private-access-service".to_string(),
            "sonicwall".to_string(),
            "--stdio".to_string(),
        ],
        version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: PrivateAccessServiceCapabilities {
            pushed_routes: true,
            pushed_dns: true,
            local_http_bridge: false,
            graceful_disconnect: true,
        },
        config_schema: json!({
            "mode": { "type": "string", "enum": ["tun"], "default": "tun" },
            "server": { "type": "string", "required": true },
            "realm": { "type": "string", "required": true, "default": "Hundsun" },
            "tun_helper": { "type": "array", "items": { "type": "string" }, "required": false },
            "http_connect_proxy": { "type": "string", "required": false },
            "http_connect_proxy_context": { "type": "string", "required": false },
            "http_connect_controller": { "type": "string", "required": false },
            "http_connect_selector": { "type": "string", "required": false },
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

#[derive(Deserialize)]
struct SonicwallServiceConfig {
    #[serde(default = "default_sonicwall_service_mode")]
    mode: HillstoneServiceMode,
    server: String,
    #[serde(default = "default_sonicwall_realm")]
    realm: String,
    #[serde(default = "default_tls_verify")]
    tls_verify: bool,
    #[serde(default)]
    tun_helper: Option<Vec<String>>,
    #[serde(default)]
    http_connect_proxy: Option<String>,
    #[serde(default)]
    http_connect_proxy_context: Option<String>,
    #[serde(default)]
    http_connect_controller: Option<String>,
    #[serde(default)]
    http_connect_selector: Option<String>,
    #[serde(default = "default_sonicwall_timeout_secs")]
    timeout_secs: u64,
}

fn default_sonicwall_realm() -> String {
    "Hundsun".to_string()
}

fn default_sonicwall_service_mode() -> HillstoneServiceMode {
    HillstoneServiceMode::Tun
}

fn default_sonicwall_timeout_secs() -> u64 {
    30
}

fn default_tls_verify() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HillstoneServiceMode {
    Bridge,
    Tun,
}

impl HillstoneServiceMode {
    fn label(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Tun => "tun",
        }
    }
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
        "sonicwall" => run_sonicwall_private_access_service_stdio(),
        value => bail!("unsupported private access service: {value}"),
    }
}

enum SonicwallAuthInput {
    Reply {
        session_id: String,
        challenge_id: String,
        button: String,
        replies: Vec<PrivateAccessSecret>,
    },
    Cancel,
}

struct SonicwallServiceSession {
    shutdown: Arc<AtomicBool>,
    auth_tx: Sender<SonicwallAuthInput>,
    worker: JoinHandle<()>,
}

fn run_sonicwall_private_access_service_stdio() -> Result<()> {
    let detached = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(JsonLineEventSink::new(
        "sonicwall",
        io::stdout(),
        Arc::clone(&detached),
    ));
    let gateway_profile_path = sonicwall_gateway_profile_cache_path();
    let gateway_profile_cache =
        match SonicwallGatewayProfileCache::load(gateway_profile_path.clone()) {
            Ok(cache) => cache,
            Err(error) => {
                append_sonicwall_diagnostic(
                    "profile",
                    &format!(
                        "gateway profile cache could not be loaded and will be reset: {error:#}"
                    ),
                );
                SonicwallGatewayProfileCache::with_path(gateway_profile_path)
            }
        };
    let gateway_profiles = Arc::new(Mutex::new(gateway_profile_cache));
    let mut session: Option<SonicwallServiceSession> = None;
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
                if service != "sonicwall" {
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
                let config: SonicwallServiceConfig = serde_json::from_value(config)
                    .context("failed to parse SonicWall private access config")?;
                session = Some(start_sonicwall_service_session(
                    config,
                    Arc::clone(&sink),
                    Arc::clone(&gateway_profiles),
                )?);
            }
            PrivateAccessCommand::AuthReply {
                service,
                session_id,
                challenge_id,
                button,
                replies,
                ..
            } => {
                if service != "sonicwall" {
                    emit_service_error(&sink, "invalid_service", "command service mismatch")?;
                    continue;
                }
                let Some(active) = session.as_ref() else {
                    emit_service_error(&sink, "no_session", "no authentication session exists")?;
                    continue;
                };
                if active
                    .auth_tx
                    .send(SonicwallAuthInput::Reply {
                        session_id,
                        challenge_id,
                        button,
                        replies,
                    })
                    .is_err()
                {
                    emit_service_error(
                        &sink,
                        "auth_session_closed",
                        "authentication session is no longer accepting replies",
                    )?;
                }
            }
            PrivateAccessCommand::CancelAuth { service, .. }
            | PrivateAccessCommand::Disconnect { service, .. } => {
                if service != "sonicwall" {
                    emit_service_error(&sink, "invalid_service", "command service mismatch")?;
                    continue;
                }
                stop_sonicwall_service_session(&sink, session.take())?;
            }
            PrivateAccessCommand::Detach { service, .. } => {
                if service != "sonicwall" {
                    emit_service_error(&sink, "invalid_service", "command service mismatch")?;
                    continue;
                }
                detached.store(true, Ordering::SeqCst);
                stop_sonicwall_service_session(&sink, session.take())?;
            }
            PrivateAccessCommand::Status { .. } => {
                let state = if session.is_some() {
                    PrivateAccessState::Connecting
                } else {
                    PrivateAccessState::Disconnected
                };
                sink.state(state, "status requested")?;
            }
        }
    }
    if let Some(active) = session.take() {
        active.shutdown.store(true, Ordering::SeqCst);
        let _ = active.auth_tx.send(SonicwallAuthInput::Cancel);
        let _ = active.worker.join();
    }
    Ok(())
}

fn stop_sonicwall_service_session(
    sink: &JsonLineEventSink<io::Stdout>,
    session: Option<SonicwallServiceSession>,
) -> Result<()> {
    if let Some(active) = session {
        sink.state(
            PrivateAccessState::Disconnecting,
            "authentication cancelled",
        )?;
        active.shutdown.store(true, Ordering::SeqCst);
        let _ = active.auth_tx.send(SonicwallAuthInput::Cancel);
        let _ = active.worker.join();
    } else {
        sink.state(PrivateAccessState::Disconnected, "no active session")?;
    }
    Ok(())
}

fn start_sonicwall_service_session(
    config: SonicwallServiceConfig,
    sink: Arc<JsonLineEventSink<io::Stdout>>,
    gateway_profiles: Arc<Mutex<SonicwallGatewayProfileCache>>,
) -> Result<SonicwallServiceSession> {
    append_sonicwall_diagnostic("session", "starting authentication session");
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_sink = Arc::clone(&sink);
    let (auth_tx, auth_rx) = mpsc::channel();
    sink.state(
        PrivateAccessState::Connecting,
        "discovering SonicWall authentication service",
    )?;
    let worker = thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = worker_sink.error("runtime_failed", &error.to_string());
                let _ = worker_sink.state(PrivateAccessState::Error, "service runtime failed");
                return;
            }
        };
        let result = runtime.block_on(run_sonicwall_authentication(
            config,
            Arc::clone(&worker_sink),
            auth_rx,
            worker_shutdown,
            gateway_profiles,
        ));
        if let Err(error) = result {
            append_sonicwall_diagnostic("error", &format!("{error:#}"));
            let _ = worker_sink.error("authentication_failed", &format!("{error:#}"));
            let _ = worker_sink.state(PrivateAccessState::Error, "authentication failed");
        }
    });
    Ok(SonicwallServiceSession {
        shutdown,
        auth_tx,
        worker,
    })
}

fn sonicwall_candidate_delays(preferred: SonicwallTransport) -> (Duration, Duration) {
    match preferred {
        SonicwallTransport::Direct => (Duration::ZERO, SONICWALL_HAPPY_EYEBALLS_DELAY),
        SonicwallTransport::Proxy => (SONICWALL_HAPPY_EYEBALLS_DELAY, Duration::ZERO),
    }
}

async fn discover_sonicwall_candidate(
    server: &str,
    tls_verify: bool,
    proxy: Option<&str>,
    transport: SonicwallTransport,
    delay: Duration,
) -> Result<(SonicwallAuthClient, Vec<crate::sonicwall::SonicwallRealm>)> {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    append_sonicwall_diagnostic(
        "transport",
        &format!("starting {} HTTPS discovery candidate", transport.label()),
    );
    let client = SonicwallAuthClient::new(server, tls_verify, proxy)?;
    let realms = client.discover_realms().await?;
    Ok((client, realms))
}

async fn discover_sonicwall_transport(
    server: &str,
    tls_verify: bool,
    fallback_proxy: Option<&str>,
    preferred: SonicwallTransport,
) -> Result<(
    SonicwallAuthClient,
    Vec<crate::sonicwall::SonicwallRealm>,
    SonicwallTransport,
)> {
    let Some(proxy) = fallback_proxy.filter(|proxy| !proxy.trim().is_empty()) else {
        append_sonicwall_diagnostic(
            "transport",
            "HTTP CONNECT proxy unavailable; probing direct HTTPS only",
        );
        let (client, realms) = discover_sonicwall_candidate(
            server,
            tls_verify,
            None,
            SonicwallTransport::Direct,
            Duration::ZERO,
        )
        .await
        .context("direct SonicWall gateway discovery failed")?;
        return Ok((client, realms, SonicwallTransport::Direct));
    };

    let (direct_delay, proxy_delay) = sonicwall_candidate_delays(preferred);
    append_sonicwall_diagnostic(
        "transport",
        &format!(
            "racing direct and proxy HTTPS candidates; preferred={}; alternate_delay_ms={}",
            preferred.label(),
            SONICWALL_HAPPY_EYEBALLS_DELAY.as_millis()
        ),
    );
    let direct = discover_sonicwall_candidate(
        server,
        tls_verify,
        None,
        SonicwallTransport::Direct,
        direct_delay,
    );
    let proxied = discover_sonicwall_candidate(
        server,
        tls_verify,
        Some(proxy),
        SonicwallTransport::Proxy,
        proxy_delay,
    );
    tokio::pin!(direct);
    tokio::pin!(proxied);

    tokio::select! {
        direct_result = &mut direct => match direct_result {
            Ok((client, realms)) => Ok((client, realms, SonicwallTransport::Direct)),
            Err(direct_error) => {
                append_sonicwall_diagnostic(
                    "transport",
                    &format!("direct discovery candidate failed: {direct_error:#}; waiting for proxy candidate"),
                );
                match proxied.await {
                    Ok((client, realms)) => Ok((client, realms, SonicwallTransport::Proxy)),
                    Err(proxy_error) => Err(anyhow::anyhow!(
                        "direct SonicWall gateway discovery failed ({direct_error:#}); HTTP CONNECT proxy discovery also failed ({proxy_error:#})"
                    )),
                }
            }
        },
        proxy_result = &mut proxied => match proxy_result {
            Ok((client, realms)) => Ok((client, realms, SonicwallTransport::Proxy)),
            Err(proxy_error) => {
                append_sonicwall_diagnostic(
                    "transport",
                    &format!("proxy discovery candidate failed: {proxy_error:#}; waiting for direct candidate"),
                );
                match direct.await {
                    Ok((client, realms)) => Ok((client, realms, SonicwallTransport::Direct)),
                    Err(direct_error) => Err(anyhow::anyhow!(
                        "HTTP CONNECT proxy discovery failed ({proxy_error:#}); direct SonicWall gateway discovery also failed ({direct_error:#})"
                    )),
                }
            }
        },
    }
}

async fn run_sonicwall_authentication(
    config: SonicwallServiceConfig,
    sink: Arc<JsonLineEventSink<io::Stdout>>,
    auth_rx: Receiver<SonicwallAuthInput>,
    shutdown: Arc<AtomicBool>,
    gateway_profiles: Arc<Mutex<SonicwallGatewayProfileCache>>,
) -> Result<()> {
    let proxy_from_env = std::env::var("SONICWALL_EVPN_PROXY").ok();
    let fallback_proxy = config
        .http_connect_proxy
        .as_deref()
        .or(proxy_from_env.as_deref());
    append_sonicwall_diagnostic(
        "transport",
        &format!(
            "configured HTTP CONNECT proxy={}; outbound_context={}",
            fallback_proxy.unwrap_or("none"),
            config
                .http_connect_proxy_context
                .as_deref()
                .unwrap_or("unknown")
        ),
    );
    let cached_profile = gateway_profiles
        .lock()
        .map_err(|_| anyhow::anyhow!("SonicWall gateway profile cache lock was poisoned"))?
        .get(&config.server);
    append_sonicwall_diagnostic(
        "profile",
        &format!(
            "gateway={} preferred_transport={} logon_capability={}",
            normalize_sonicwall_gateway_cache_key(&config.server),
            cached_profile.transport.label(),
            cached_profile.logon_capability.label()
        ),
    );
    let (client, realms, selected_transport) = discover_sonicwall_transport(
        &config.server,
        config.tls_verify,
        fallback_proxy,
        cached_profile.transport,
    )
    .await?;
    {
        let mut profiles = gateway_profiles
            .lock()
            .map_err(|_| anyhow::anyhow!("SonicWall gateway profile cache lock was poisoned"))?;
        profiles.update_transport(&config.server, selected_transport);
        if let Err(error) = profiles.persist() {
            append_sonicwall_diagnostic(
                "profile",
                &format!("failed to persist transport preference: {error:#}"),
            );
        }
    }
    append_sonicwall_diagnostic(
        "transport",
        &format!(
            "selected {} HTTPS transport via Happy Eyeballs",
            selected_transport.label()
        ),
    );
    let selected_proxy = (selected_transport == SonicwallTransport::Proxy)
        .then_some(fallback_proxy)
        .flatten();
    if !realms.is_empty() && !realms.iter().any(|realm| realm.name == config.realm) {
        let names = realms
            .iter()
            .map(|realm| realm.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "configured SonicWall LoginGroup {} was not advertised; available groups: {names}",
            config.realm
        );
    }
    if shutdown.load(Ordering::SeqCst) {
        sink.state(PrivateAccessState::Disconnected, "authentication cancelled")?;
        return Ok(());
    }

    let evpn_fallback_proxy = selected_proxy.is_none().then_some(fallback_proxy).flatten();
    let mut preferred_logon_capability = cached_profile.logon_capability;
    let mut reauthentication_sequence = 0_u64;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            sink.state(PrivateAccessState::Disconnected, "authentication cancelled")?;
            return Ok(());
        }
        let session = client
            .start_logon(&config.realm, preferred_logon_capability)
            .await?;
        preferred_logon_capability = session.logon_capability();
        {
            let mut profiles = gateway_profiles.lock().map_err(|_| {
                anyhow::anyhow!("SonicWall gateway profile cache lock was poisoned")
            })?;
            profiles.update_logon_capability(&config.server, preferred_logon_capability);
            if let Err(error) = profiles.persist() {
                append_sonicwall_diagnostic(
                    "profile",
                    &format!("failed to persist logon capability: {error:#}"),
                );
            }
        }
        let official_status = session
            .official_logon_status()
            .map(|status| status.to_string())
            .unwrap_or_else(|| "cached-skip".to_string());
        append_sonicwall_diagnostic(
            "authentication",
            &format!(
                "logon endpoint: {}; official_status={}; reauthentication_sequence={reauthentication_sequence}",
                session.logon_endpoint(),
                official_status
            ),
        );
        let result = run_sonicwall_auth_dialog(
            &session,
            &config,
            Arc::clone(&sink),
            &auth_rx,
            Arc::clone(&shutdown),
            selected_proxy,
            evpn_fallback_proxy,
        )
        .await;
        let cancelled = shutdown.load(Ordering::SeqCst);
        let reauthentication_reason = result.as_ref().err().and_then(|error| {
            error
                .downcast_ref::<SonicwallReauthenticationRequired>()
                .map(|error| error.reason.clone())
        });
        if cancelled {
            let _ = session.close().await;
            return Ok(());
        }
        if let Some(reason) = reauthentication_reason {
            reauthentication_sequence = reauthentication_sequence.saturating_add(1);
            append_sonicwall_diagnostic(
                "authentication",
                &format!(
                    "authenticated session expired; starting interactive reauthentication #{reauthentication_sequence}; reason={reason}"
                ),
            );
            match tokio::time::timeout(Duration::from_secs(3), session.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => append_sonicwall_diagnostic(
                    "authentication",
                    &format!("expired session cleanup failed and was ignored: {error:#}"),
                ),
                Err(_) => append_sonicwall_diagnostic(
                    "authentication",
                    "expired session cleanup exceeded 3 seconds and was abandoned",
                ),
            }
            sink.state(
                PrivateAccessState::Connecting,
                "SonicWall session expired; fresh authentication and dynamic code are required",
            )?;
            continue;
        }
        let cleanup = session.close().await;
        return match (result, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => {
                Err(error.context("authentication completed but cleanup failed"))
            }
            (Ok(()), Ok(())) => Ok(()),
        };
    }
}

async fn run_sonicwall_auth_dialog(
    session: &SonicwallAuthSession,
    config: &SonicwallServiceConfig,
    sink: Arc<JsonLineEventSink<io::Stdout>>,
    auth_rx: &Receiver<SonicwallAuthInput>,
    shutdown: Arc<AtomicBool>,
    evpn_primary_proxy: Option<&str>,
    evpn_fallback_proxy: Option<&str>,
) -> Result<()> {
    let _micro_interrogation = session.get_agent_info().await?;
    let micro_step = session
        .post_agent_info(&sonicwall_default_agent_info())
        .await?;
    let initial_step = sonicwall_auth_step_name(&micro_step);
    append_sonicwall_diagnostic(
        "authentication",
        &format!("micro interrogation endpoint: agentinfo; initial_step={initial_step}"),
    );
    let empty_replies = Vec::new();
    let mut step = match micro_step {
        SonicwallAuthStep::Continue => session.authenticate("ok", &empty_replies).await?,
        step => step,
    };
    let session_id = format!("auth-{:016x}", rand::random::<u64>());
    let mut challenge_sequence = 0_u64;

    loop {
        match step {
            SonicwallAuthStep::Challenge(challenge) => {
                append_sonicwall_diagnostic(
                    "authentication",
                    &format!(
                        "received challenge with {} field(s)",
                        challenge.fields.len()
                    ),
                );
                challenge_sequence = challenge_sequence.saturating_add(1);
                let challenge_id = format!("challenge-{challenge_sequence}");
                sink.emit(PrivateAccessEvent::AuthChallenge {
                    service: "sonicwall".to_string(),
                    session_id: session_id.clone(),
                    challenge_id: challenge_id.clone(),
                    title: challenge.title,
                    message: challenge.message,
                    fields: challenge.fields,
                    buttons: challenge.buttons,
                })?;
                let Some((button, replies)) = wait_for_sonicwall_auth_reply(
                    auth_rx,
                    shutdown.as_ref(),
                    &session_id,
                    &challenge_id,
                )?
                else {
                    sink.state(PrivateAccessState::Disconnected, "authentication cancelled")?;
                    return Ok(());
                };
                step = session.authenticate(&button, &replies).await?;
            }
            SonicwallAuthStep::Authenticated => {
                append_sonicwall_diagnostic(
                    "authentication",
                    "credentials accepted; starting EVPN bootstrap",
                );
                match session.probe_system_interrogation().await {
                    Ok(interrogation) => append_sonicwall_diagnostic(
                        "authentication",
                        &format!(
                            "system interrogation endpoint: {}; zone_count={:?}; zone_keys={}; unsupported_zone_keys={}; posted_minimal_response={}; is_ct_allow={:?}; zoneCommand={}; zoneType={}",
                            interrogation.endpoint.unwrap_or("not found"),
                            interrogation.zone_count,
                            if interrogation.zone_keys.is_empty() {
                                "none".to_string()
                            } else {
                                interrogation.zone_keys.join(",")
                            },
                            if interrogation.unsupported_zone_keys.is_empty() {
                                "none".to_string()
                            } else {
                                interrogation.unsupported_zone_keys.join(",")
                            },
                            interrogation.posted_minimal_response,
                            interrogation.is_ct_allow,
                            interrogation.zone_command.as_deref().unwrap_or("unknown"),
                            interrogation.zone_type.as_deref().unwrap_or("unknown")
                        ),
                    ),
                    Err(error) => append_sonicwall_diagnostic(
                        "authentication",
                        &format!("system interrogation probe failed: {error:#}"),
                    ),
                }
                match session.probe_license_state().await {
                    Ok(license) => append_sonicwall_diagnostic(
                        "authentication",
                        &format!(
                            "license endpoint: {}; licensed={:?}; destroy_connections={:?}; status={}",
                            license.endpoint.unwrap_or("not found"),
                            license.licensed,
                            license.destroy_connections,
                            license.status.as_deref().unwrap_or("unknown")
                        ),
                    ),
                    Err(error) => append_sonicwall_diagnostic(
                        "authentication",
                        &format!("license probe failed: {error:#}"),
                    ),
                }
                match session.probe_connection_state().await {
                    Ok(state) => append_sonicwall_diagnostic(
                        "authentication",
                        &format!(
                            "connection state endpoint: {}; ALPNSupported={:?}; tunnelProtocolNegotiation={:?}; zoneType={}",
                            state.endpoint.unwrap_or("not found"),
                            state.alpns_supported,
                            state.tunnel_protocol_negotiation,
                            state.zone_type.as_deref().unwrap_or("unknown")
                        ),
                    ),
                    Err(error) => append_sonicwall_diagnostic(
                        "authentication",
                        &format!("connection state probe failed: {error:#}"),
                    ),
                }
                let agent_activation = session.activate_connect_tunnel_agent().await?;
                append_sonicwall_diagnostic(
                    "authentication",
                    &format!(
                        "ConnectTunnel agent activation endpoint: {}",
                        agent_activation.endpoint.unwrap_or("not found")
                    ),
                );
                sink.state(
                    PrivateAccessState::Connecting,
                    "SonicWall authentication complete; establishing EVPN tunnel",
                )?;
                if config.mode != HillstoneServiceMode::Tun {
                    bail!("SonicWall clean-room client requires TUN mode");
                }
                return run_sonicwall_authenticated_tunnel(
                    session,
                    config,
                    sink,
                    shutdown,
                    &session_id,
                    evpn_primary_proxy,
                    evpn_fallback_proxy,
                )
                .await;
            }
            SonicwallAuthStep::Continue => {
                bail!("SonicWall returned an unrecognized authentication response");
            }
        }
    }
}

#[derive(Deserialize)]
struct SonicwallControllerProxies {
    proxies: HashMap<String, SonicwallControllerProxy>,
}

#[derive(Deserialize)]
struct SonicwallControllerProxy {
    #[serde(default)]
    now: Option<String>,
}

fn sonicwall_outbound_chain(
    proxies: &HashMap<String, SonicwallControllerProxy>,
    root_selector: &str,
) -> Option<String> {
    let mut current = root_selector;
    let mut chain = vec![current.to_string()];
    let mut visited = vec![current.to_string()];
    loop {
        let selected = proxies.get(current)?.now.as_deref()?;
        chain.push(selected.to_string());
        let Some(next) = proxies.get(selected) else {
            break;
        };
        if visited.iter().any(|name| name == selected) {
            chain.push("(cycle)".to_string());
            break;
        }
        visited.push(selected.to_string());
        current = selected;
        if next.now.is_none() {
            break;
        }
    }
    Some(chain.join(" -> "))
}

async fn current_sonicwall_outbound_context(config: &SonicwallServiceConfig) -> String {
    let fallback = config
        .http_connect_proxy_context
        .as_deref()
        .unwrap_or("unknown")
        .to_string();
    let Some(controller) = config
        .http_connect_controller
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return fallback;
    };
    let Some(root_selector) = config
        .http_connect_selector
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return fallback;
    };
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            append_sonicwall_diagnostic(
                "transport",
                &format!("could not build live outbound query client: {error}"),
            );
            return fallback;
        }
    };
    let mut request = client.get(format!("{}/proxies", controller.trim_end_matches('/')));
    if let Ok(secret) = std::env::var("SING_BOX_SECRET")
        && !secret.trim().is_empty()
    {
        request = request.bearer_auth(secret);
    }
    let result = async {
        let response = request
            .send()
            .await
            .context("failed to query sing-box controller /proxies")?
            .error_for_status()
            .context("sing-box controller /proxies returned an error")?;
        let payload = response
            .json::<SonicwallControllerProxies>()
            .await
            .context("failed to decode sing-box controller /proxies")?;
        sonicwall_outbound_chain(&payload.proxies, root_selector)
            .context("configured root selector was not present in /proxies")
    }
    .await;
    match result {
        Ok(context) => context,
        Err(error) => {
            append_sonicwall_diagnostic(
                "transport",
                &format!(
                    "live outbound context lookup failed; using connection-time snapshot; error={error:#}"
                ),
            );
            fallback
        }
    }
}

fn establish_sonicwall_evpn(
    identity: &SonicwallEvpnIdentity,
    config: &SonicwallServiceConfig,
    guid: [u8; 16],
    primary_proxy: Option<&str>,
    fallback_proxy: Option<&str>,
    outbound_context: &str,
) -> Result<EstablishedEvpn> {
    let trace_evpn = |message: &str| append_sonicwall_diagnostic("evpn", message);
    let connect_evpn = |http_connect_proxy| {
        connect_sonicwall_evpn(&EvpnBootstrapOptions {
            server: &identity.server,
            port: identity.port,
            http_connect_proxy,
            timeout: Duration::from_secs(config.timeout_secs),
            verify_server_cert: config.tls_verify,
            team_token: identity.team_token.expose(),
            guid,
            trace: Some(&trace_evpn),
        })
    };
    append_sonicwall_diagnostic(
        "transport",
        &format!(
            "establishing EVPN underlay={} proxy={} outbound_context={}",
            if primary_proxy.is_some() {
                "http-connect"
            } else {
                "direct"
            },
            primary_proxy.unwrap_or("none"),
            outbound_context
        ),
    );
    match connect_evpn(primary_proxy) {
        Ok(established) => Ok(established),
        Err(direct_error) if primary_proxy.is_none() && fallback_proxy.is_some() => {
            append_sonicwall_diagnostic(
                "transport",
                &format!(
                    "direct EVPN bootstrap failed: {direct_error:#}; retrying through configured HTTP CONNECT proxy"
                ),
            );
            let proxy = fallback_proxy.expect("EVPN fallback proxy was checked as present");
            let established = connect_evpn(Some(proxy)).map_err(|proxy_error| {
                anyhow::anyhow!(
                    "direct SonicWall EVPN bootstrap failed ({direct_error:#}); HTTP CONNECT proxy fallback also failed ({proxy_error:#})"
                )
            })?;
            append_sonicwall_diagnostic(
                "transport",
                &format!(
                    "selected HTTP CONNECT proxy fallback for EVPN; proxy={proxy}; outbound_context={}",
                    outbound_context
                ),
            );
            Ok(established)
        }
        Err(error) => Err(error),
    }
}

async fn run_sonicwall_authenticated_tunnel(
    session: &SonicwallAuthSession,
    config: &SonicwallServiceConfig,
    sink: Arc<JsonLineEventSink<io::Stdout>>,
    shutdown: Arc<AtomicBool>,
    session_id: &str,
    evpn_primary_proxy: Option<&str>,
    evpn_fallback_proxy: Option<&str>,
) -> Result<()> {
    let guid = rand::random::<[u8; 16]>();
    let identity = session.evpn_identity()?;
    append_sonicwall_diagnostic(
        "authentication",
        &format!(
            "using EVPN logon token after {} refresh(es), {} observation(s)",
            identity.logon_id_refresh_count, identity.logon_id_observation_count
        ),
    );
    let outbound_context = current_sonicwall_outbound_context(config).await;
    let initial = establish_sonicwall_evpn(
        &identity,
        config,
        guid,
        evpn_primary_proxy,
        evpn_fallback_proxy,
        &outbound_context,
    )
    .map_err(|error| {
        if is_sonicwall_team_auth_error(&error) {
            sonicwall_reauthentication_required(
                "the gateway rejected the EVPN TEAM token during initial bootstrap",
            )
        } else {
            error
        }
    })?;
    let mut established = Some(initial);
    let mut reconnect_failures = 0_usize;
    let mut last_transport_error: Option<anyhow::Error> = None;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(data_plane) = established.take() {
            let connected_at = Instant::now();
            match supervise_sonicwall_tun_data_plane(
                session,
                data_plane,
                config.tun_helper.clone(),
                Arc::clone(&sink),
                Arc::clone(&shutdown),
                session_id,
            )
            .await
            {
                Ok(()) => return Ok(()),
                Err(error) if is_sonicwall_transport_disconnect(&error) => {
                    let uptime = connected_at.elapsed();
                    if uptime >= SONICWALL_RAPID_DISCONNECT_WINDOW {
                        reconnect_failures = 0;
                    }
                    append_sonicwall_diagnostic(
                        "reconnect",
                        &format!(
                            "recoverable EVPN transport loss after {:.3}s: {error:#}",
                            uptime.as_secs_f64()
                        ),
                    );
                    last_transport_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        if reconnect_failures >= SONICWALL_RECONNECT_BACKOFFS.len() {
            let error = last_transport_error
                .take()
                .unwrap_or_else(|| anyhow::anyhow!("SonicWall EVPN transport disconnected"));
            return Err(error).context(format!(
                "SonicWall EVPN reconnect budget exhausted after {} attempt(s)",
                SONICWALL_RECONNECT_BACKOFFS.len()
            ));
        }

        let attempt = reconnect_failures + 1;
        let backoff = SONICWALL_RECONNECT_BACKOFFS[reconnect_failures];
        reconnect_failures += 1;
        sink.state(
            PrivateAccessState::Connecting,
            &format!(
                "SonicWall data tunnel disconnected; reconnecting with the existing authenticated session ({attempt}/{})",
                SONICWALL_RECONNECT_BACKOFFS.len()
            ),
        )?;
        append_sonicwall_diagnostic(
            "reconnect",
            &format!(
                "attempt {attempt}/{} scheduled after {:.3}s; credentials and OTP will not be replayed",
                SONICWALL_RECONNECT_BACKOFFS.len(),
                backoff.as_secs_f64()
            ),
        );

        match session.probe_connection_state().await {
            Ok(state) if state.endpoint.is_none() => {
                append_sonicwall_diagnostic(
                    "control",
                    "pre-reconnect state resource is missing; skipping obsolete-token retries",
                );
                return Err(sonicwall_reauthentication_required(
                    "the SonicWall control-session state resource no longer exists",
                ));
            }
            Ok(state) => append_sonicwall_diagnostic(
                "control",
                &format!(
                    "pre-reconnect state refresh succeeded; endpoint={}; zoneType={}",
                    state.endpoint.unwrap_or("not found"),
                    state.zone_type.as_deref().unwrap_or("unknown")
                ),
            ),
            Err(error) => append_sonicwall_diagnostic(
                "control",
                &format!("pre-reconnect state refresh failed: {error:#}"),
            ),
        }
        tokio::time::sleep(backoff).await;
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }

        let identity = match session.evpn_identity() {
            Ok(identity) => identity,
            Err(error) if is_sonicwall_team_auth_error(&error) => {
                append_sonicwall_diagnostic(
                    "reconnect",
                    &format!(
                        "attempt {attempt} was rejected with TEAM AUTH; switching immediately to interactive reauthentication"
                    ),
                );
                return Err(sonicwall_reauthentication_required(
                    "the gateway rejected the existing EVPN TEAM token",
                ));
            }
            Err(error) => {
                append_sonicwall_diagnostic(
                    "reconnect",
                    &format!(
                        "attempt {attempt} could not obtain the current EVPN token: {error:#}"
                    ),
                );
                last_transport_error = Some(error);
                continue;
            }
        };
        append_sonicwall_diagnostic(
            "reconnect",
            &format!(
                "attempt {attempt} is using EVPN token after {} refresh(es), {} observation(s)",
                identity.logon_id_refresh_count, identity.logon_id_observation_count
            ),
        );
        let outbound_context = current_sonicwall_outbound_context(config).await;
        match establish_sonicwall_evpn(
            &identity,
            config,
            guid,
            evpn_primary_proxy,
            evpn_fallback_proxy,
            &outbound_context,
        ) {
            Ok(data_plane) => {
                append_sonicwall_diagnostic(
                    "reconnect",
                    &format!("attempt {attempt} re-established the EVPN tunnel"),
                );
                established = Some(data_plane);
            }
            Err(error) => {
                append_sonicwall_diagnostic(
                    "reconnect",
                    &format!("attempt {attempt} failed: {error:#}"),
                );
                last_transport_error = Some(error);
            }
        }
    }
}

async fn supervise_sonicwall_tun_data_plane(
    session: &SonicwallAuthSession,
    established: EstablishedEvpn,
    tun_helper: Option<Vec<String>>,
    sink: Arc<JsonLineEventSink<io::Stdout>>,
    shutdown: Arc<AtomicBool>,
    session_id: &str,
) -> Result<()> {
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let worker_sink = Arc::clone(&sink);
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_session_id = session_id.to_string();
    let worker = thread::spawn(move || {
        let result = run_sonicwall_tun_data_plane(
            established,
            tun_helper,
            worker_sink.as_ref(),
            worker_shutdown.as_ref(),
            &worker_session_id,
        );
        let _ = result_tx.send(result);
    });
    let mut next_control_keepalive = Instant::now() + SONICWALL_CONTROL_KEEPALIVE_INTERVAL;
    let mut keepalive_sequence = 0_u64;
    let mut consecutive_keepalive_failures = 0_u64;

    loop {
        match result_rx.try_recv() {
            Ok(result) => {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("SonicWall TUN data-plane worker panicked"))?;
                return result;
            }
            Err(TryRecvError::Disconnected) => {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("SonicWall TUN data-plane worker panicked"))?;
                bail!("SonicWall TUN data-plane worker stopped without a result");
            }
            Err(TryRecvError::Empty) => {}
        }

        if Instant::now() >= next_control_keepalive {
            keepalive_sequence = keepalive_sequence.saturating_add(1);
            let started = Instant::now();
            match session.probe_connection_state().await {
                Ok(state) => {
                    consecutive_keepalive_failures = 0;
                    append_sonicwall_diagnostic(
                        "control",
                        &format!(
                            "state keepalive #{keepalive_sequence} succeeded in {:.3}s; endpoint={}; zoneType={}",
                            started.elapsed().as_secs_f64(),
                            state.endpoint.unwrap_or("not found"),
                            state.zone_type.as_deref().unwrap_or("unknown")
                        ),
                    );
                }
                Err(error) => {
                    consecutive_keepalive_failures =
                        consecutive_keepalive_failures.saturating_add(1);
                    append_sonicwall_diagnostic(
                        "control",
                        &format!(
                            "state keepalive #{keepalive_sequence} failed in {:.3}s; consecutive_failures={consecutive_keepalive_failures}; error={error:#}",
                            started.elapsed().as_secs_f64()
                        ),
                    );
                }
            }
            next_control_keepalive = Instant::now() + SONICWALL_CONTROL_KEEPALIVE_INTERVAL;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn is_sonicwall_transport_disconnect(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "gateway closed the data tunnel",
        "failed to read sonicwall evpn data tunnel",
        "failed to send an ipv4 packet through sonicwall evpn",
        "failed to answer sonicwall evpn keepalive",
        "failed to send sonicwall evpn keepalive",
        "connection reset",
        "connection aborted",
        "broken pipe",
        "unexpected eof",
        "os error 10053",
        "os error 10054",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

fn is_sonicwall_team_auth_error(error: &anyhow::Error) -> bool {
    format!("{error:#}")
        .to_ascii_lowercase()
        .contains("team auth error")
}

fn sonicwall_auth_step_name(step: &SonicwallAuthStep) -> &'static str {
    match step {
        SonicwallAuthStep::Challenge(_) => "challenge",
        SonicwallAuthStep::Authenticated => "authenticated",
        SonicwallAuthStep::Continue => "continue",
    }
}

fn wait_for_sonicwall_auth_reply(
    auth_rx: &Receiver<SonicwallAuthInput>,
    shutdown: &AtomicBool,
    expected_session_id: &str,
    expected_challenge_id: &str,
) -> Result<Option<(String, Vec<PrivateAccessSecret>)>> {
    while !shutdown.load(Ordering::SeqCst) {
        match auth_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(SonicwallAuthInput::Cancel) => return Ok(None),
            Ok(SonicwallAuthInput::Reply {
                session_id,
                challenge_id,
                button,
                replies,
            }) => {
                if session_id != expected_session_id || challenge_id != expected_challenge_id {
                    continue;
                }
                return Ok(Some((button, replies)));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
    Ok(None)
}

fn run_sonicwall_tun_data_plane(
    mut evpn: EstablishedEvpn,
    tun_helper: Option<Vec<String>>,
    sink: &JsonLineEventSink<io::Stdout>,
    shutdown: &AtomicBool,
    session_id: &str,
) -> Result<()> {
    let assigned_ipv4 = evpn.config.assigned_ipv4;
    if assigned_ipv4.is_unspecified() {
        bail!("SonicWall EVPN gateway did not assign an IPv4 address");
    }
    let route_cidrs = sonicwall_route_cidrs(&evpn.config);
    let dns_servers = evpn.config.dns.clone();
    let domains = sonicwall_resource_domains(&evpn.config);
    let domain_suffixes = sonicwall_domain_suffixes(&evpn.config);
    let mtu = evpn.config.ssl_mtu.unwrap_or(1428).clamp(1200, 1500);
    let mut tun = TunHelperClient::spawn(
        tun_helper,
        TunHelperStartConfig {
            client_ipv4: assigned_ipv4,
            gateway_ipv4: None,
            prefix_len: u32::from(evpn.config.ipv4_prefix_len.clamp(1, 32)),
            route_cidrs: route_cidrs.clone(),
            dns_servers: dns_servers.clone(),
            mtu: Some(mtu),
        },
    )?;
    let interface = tun.interface().to_string();
    sink.emit(PrivateAccessEvent::RoutesPushed {
        service: "sonicwall".to_string(),
        session_id: Some(session_id.to_string()),
        routes: route_cidrs
            .iter()
            .cloned()
            .map(|cidr| PrivateAccessRoute { cidr })
            .collect(),
        dns: dns_servers.iter().map(ToString::to_string).collect(),
        domains,
        domain_suffixes,
        bridge: None,
    })?;
    sink.state(
        PrivateAccessState::Connected,
        &format!(
            "SonicWall TUN data plane connected on {interface} as {assigned_ipv4} (tunnel {})",
            evpn.tunnel_id
        ),
    )?;
    append_sonicwall_diagnostic("tunnel", "TUN data plane connected");

    evpn.stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)))
        .context("failed to configure SonicWall EVPN data-plane read timeout")?;
    let loop_result = run_sonicwall_packet_loop(&mut evpn, &mut tun, shutdown, mtu);
    let stop_result = tun.stop().context("failed to stop SonicWall TUN helper");
    if shutdown.load(Ordering::SeqCst) {
        append_sonicwall_diagnostic("tunnel", "session disconnected cleanly");
        sink.state(
            PrivateAccessState::Disconnected,
            "SonicWall tunnel disconnected",
        )?;
        return stop_result;
    }
    loop_result.and(stop_result)
}

fn append_sonicwall_diagnostic(stage: &str, message: &str) {
    append_private_access_diagnostic(SONICWALL_DIAGNOSTIC_LOG, stage, message);
}

fn append_hillstone_diagnostic(stage: &str, message: &str) {
    append_private_access_diagnostic(HILLSTONE_DIAGNOSTIC_LOG, stage, message);
}

fn append_private_access_diagnostic(path: &str, stage: &str, message: &str) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let message = message
        .replace(['\r', '\n'], " ")
        .chars()
        .take(4096)
        .collect::<String>();
    if let Ok(mut log) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(log, "{timestamp} [{stage}] {message}");
    }
}

struct SonicwallTunnelActivity {
    started: Instant,
    last_inbound: Option<Instant>,
    last_outbound: Option<Instant>,
    outbound_packets: u64,
    inbound_frames: u64,
    inbound_data_packets: u64,
    echo_requests_sent: u64,
    echo_requests_answered: u64,
    echo_responses_received: u64,
}

impl SonicwallTunnelActivity {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_inbound: None,
            last_outbound: None,
            outbound_packets: 0,
            inbound_frames: 0,
            inbound_data_packets: 0,
            echo_requests_sent: 0,
            echo_requests_answered: 0,
            echo_responses_received: 0,
        }
    }

    fn summary(&self) -> String {
        let now = Instant::now();
        format!(
            "elapsed={:.3}s outbound_packets={} inbound_frames={} inbound_data_packets={} echo_requests_sent={} echo_requests_answered={} echo_responses_received={} last_inbound={} last_outbound={}",
            self.started.elapsed().as_secs_f64(),
            self.outbound_packets,
            self.inbound_frames,
            self.inbound_data_packets,
            self.echo_requests_sent,
            self.echo_requests_answered,
            self.echo_responses_received,
            sonicwall_activity_age(now, self.last_inbound),
            sonicwall_activity_age(now, self.last_outbound)
        )
    }
}

fn sonicwall_activity_age(now: Instant, activity: Option<Instant>) -> String {
    activity
        .map(|activity| format!("{:.3}s_ago", now.duration_since(activity).as_secs_f64()))
        .unwrap_or_else(|| "never".to_string())
}

fn run_sonicwall_packet_loop(
    evpn: &mut EstablishedEvpn,
    tun: &mut TunHelperClient,
    shutdown: &AtomicBool,
    mtu: u16,
) -> Result<()> {
    let mut read_buffer = vec![0_u8; 64 * 1024];
    let mut next_keepalive = Instant::now() + Duration::from_secs(30);
    let mut next_diagnostic = Instant::now() + SONICWALL_TUNNEL_DIAGNOSTIC_INTERVAL;
    let mut activity = SonicwallTunnelActivity::new();
    while !shutdown.load(Ordering::SeqCst) {
        let mut made_progress = false;
        while let Some(packet) = tun.try_recv_ipv4()? {
            activity.outbound_packets = activity.outbound_packets.saturating_add(1);
            if activity.outbound_packets <= 8 {
                append_sonicwall_diagnostic(
                    "tunnel",
                    &format!(
                        "outbound packet #{}: {}",
                        activity.outbound_packets,
                        describe_ipv4_packet(&packet)
                    ),
                );
            }
            if let Err(error) = evpn.stream.write_all(&encode_sonicwall_packet(&packet)?) {
                append_sonicwall_diagnostic(
                    "tunnel",
                    &format!("data_write_error; {}; error={error}", activity.summary()),
                );
                return Err(error).context("failed to send an IPv4 packet through SonicWall EVPN");
            }
            activity.last_outbound = Some(Instant::now());
            made_progress = true;
        }

        match evpn.stream.read(&mut read_buffer) {
            Ok(0) => {
                append_sonicwall_diagnostic(
                    "tunnel",
                    &format!("remote_eof; {}", activity.summary()),
                );
                bail!("SonicWall EVPN gateway closed the data tunnel")
            }
            Ok(length) => {
                evpn.decoder.push(&read_buffer[..length]);
                activity.last_inbound = Some(Instant::now());
                made_progress = true;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                append_sonicwall_diagnostic(
                    "tunnel",
                    &format!("socket_read_error; {}; error={error}", activity.summary()),
                );
                return Err(error).context("failed to read SonicWall EVPN data tunnel");
            }
        }
        while let Some(frame) = evpn.decoder.next_frame()? {
            activity.inbound_frames = activity.inbound_frames.saturating_add(1);
            if activity.inbound_frames <= 8 {
                append_sonicwall_diagnostic(
                    "tunnel",
                    &format!(
                        "inbound frame #{}: {}({}) flags=0x{:02x} payload_len={}",
                        activity.inbound_frames,
                        frame.message_type.name(),
                        frame.message_type.value(),
                        frame.flags,
                        frame.payload.len()
                    ),
                );
            }
            match frame.message_type {
                MessageType::DATA => {
                    activity.inbound_data_packets = activity.inbound_data_packets.saturating_add(1);
                    let packet = decode_sonicwall_packet(&frame, usize::from(mtu))?;
                    let _ = tun.send_ipv4(&packet)?;
                }
                MessageType::ECHO_REQ => {
                    activity.echo_requests_answered =
                        activity.echo_requests_answered.saturating_add(1);
                    if let Err(error) = evpn.stream.write_all(&encode_sonicwall_frame(
                        MessageType::ECHO_RSP,
                        0,
                        &frame.payload,
                    )?) {
                        append_sonicwall_diagnostic(
                            "tunnel",
                            &format!(
                                "echo_response_write_error; {}; error={error}",
                                activity.summary()
                            ),
                        );
                        return Err(error).context("failed to answer SonicWall EVPN keepalive");
                    }
                    activity.last_outbound = Some(Instant::now());
                }
                MessageType::ECHO_RSP => {
                    activity.echo_responses_received =
                        activity.echo_responses_received.saturating_add(1);
                }
                MessageType::SHUTDOWN | MessageType::ALERT => {
                    append_sonicwall_diagnostic(
                        "tunnel",
                        &format!(
                            "gateway_termination_frame type={}({}) payload_len={}; {}",
                            frame.message_type.name(),
                            frame.message_type.value(),
                            frame.payload.len(),
                            activity.summary()
                        ),
                    );
                    bail!("SonicWall EVPN gateway terminated the data tunnel")
                }
                _ => {}
            }
            made_progress = true;
        }

        if Instant::now() >= next_keepalive {
            if let Err(error) =
                evpn.stream
                    .write_all(&encode_sonicwall_frame(MessageType::ECHO_REQ, 0, &[])?)
            {
                append_sonicwall_diagnostic(
                    "tunnel",
                    &format!(
                        "echo_request_write_error; {}; error={error}",
                        activity.summary()
                    ),
                );
                return Err(error).context("failed to send SonicWall EVPN keepalive");
            }
            activity.echo_requests_sent = activity.echo_requests_sent.saturating_add(1);
            activity.last_outbound = Some(Instant::now());
            next_keepalive = Instant::now() + Duration::from_secs(30);
            made_progress = true;
        }
        if Instant::now() >= next_diagnostic {
            append_sonicwall_diagnostic("heartbeat", &activity.summary());
            next_diagnostic = Instant::now() + SONICWALL_TUNNEL_DIAGNOSTIC_INTERVAL;
        }
        if !made_progress {
            thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(())
}

fn describe_ipv4_packet(packet: &[u8]) -> String {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return format!("len={} non-IPv4-or-truncated", packet.len());
    }
    let source = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let destination = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    format!(
        "len={} protocol={} source={} destination={}",
        packet.len(),
        packet[9],
        source,
        destination
    )
}

fn sonicwall_route_cidrs(config: &SonicwallNetworkConfig) -> Vec<String> {
    let mut routes = config
        .resources
        .iter()
        .flat_map(|resource| extract_ipv4_cidrs(resource))
        .collect::<Vec<_>>();
    routes.sort();
    routes.dedup();
    if routes.is_empty() {
        // Hundsun's observed assignment is 10.22.x. The broad private route is installed only on
        // the ephemeral TUN interface and is removed with that interface on disconnect.
        routes.push("10.0.0.0/8".to_string());
    }
    routes
}

fn sonicwall_resource_domains(config: &SonicwallNetworkConfig) -> Vec<String> {
    let mut domains = config
        .resources
        .iter()
        .filter_map(|resource| {
            let resource = String::from_utf8_lossy(resource);
            let (kind, value) = resource.split_once('=')?;
            kind.eq_ignore_ascii_case("HOSTNAME")
                .then(|| normalize_domain(value))
                .flatten()
        })
        .collect::<Vec<_>>();
    domains.sort();
    domains.dedup();
    domains
}

fn sonicwall_domain_suffixes(config: &SonicwallNetworkConfig) -> Vec<String> {
    let mut suffixes = config
        .suffixes
        .iter()
        .filter_map(|suffix| normalize_domain(&String::from_utf8_lossy(suffix)))
        .collect::<Vec<_>>();
    suffixes.sort();
    suffixes.dedup();
    suffixes
}

fn normalize_domain(value: &str) -> Option<String> {
    let value = value
        .trim_matches(|character: char| character == '\0' || character.is_ascii_whitespace())
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 253
        || value.parse::<IpAddr>().is_ok()
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return None;
    }
    Some(value)
}

fn extract_ipv4_cidrs(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let text =
        text.trim_matches(|character: char| character == '\0' || character.is_ascii_whitespace());
    if let Some((kind, value)) = text.split_once('=')
        && kind.eq_ignore_ascii_case("RANGE")
    {
        return value
            .split_once(',')
            .and_then(|(start, end)| {
                Some(ipv4_range_to_cidrs(
                    start.trim().parse::<Ipv4Addr>().ok()?,
                    end.trim().parse::<Ipv4Addr>().ok()?,
                ))
            })
            .unwrap_or_default();
    }

    text.split(|character: char| {
        !(character.is_ascii_digit() || character == '.' || character == '/')
    })
    .filter_map(normalize_ipv4_cidr)
    .collect()
}

fn ipv4_range_to_cidrs(start: Ipv4Addr, end: Ipv4Addr) -> Vec<String> {
    let mut current = u64::from(u32::from(start));
    let end = u64::from(u32::from(end));
    if current > end {
        return Vec::new();
    }

    let mut cidrs = Vec::new();
    while current <= end {
        let address = current as u32;
        let aligned_host_bits = address.trailing_zeros();
        let remaining = end - current + 1;
        let remaining_host_bits = 63 - remaining.leading_zeros();
        let host_bits = aligned_host_bits.min(remaining_host_bits);
        let prefix_len = 32 - host_bits;
        cidrs.push(format!("{}/{}", Ipv4Addr::from(address), prefix_len));
        current += 1_u64 << host_bits;
    }
    cidrs
}

fn normalize_ipv4_cidr(value: &str) -> Option<String> {
    let (address, prefix) = value.split_once('/')?;
    let address = address.parse::<Ipv4Addr>().ok()?;
    let prefix = prefix.parse::<u8>().ok()?;
    (prefix <= 32).then(|| format!("{address}/{prefix}"))
}

fn run_hillstone_private_access_service_stdio() -> Result<()> {
    append_hillstone_diagnostic(
        "service",
        &format!(
            "Hillstone Private Access service started; pid={}",
            std::process::id()
        ),
    );
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
                append_hillstone_diagnostic("command", "disconnect requested");
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
                append_hillstone_diagnostic("command", "service detached from TUI");
                detached.store(true, Ordering::SeqCst);
                let state = if session.is_some() {
                    PrivateAccessState::Connected
                } else {
                    PrivateAccessState::Disconnected
                };
                sink.state(state, "service detached from TUI")?;
            }
            PrivateAccessCommand::Status { .. } => {
                append_hillstone_diagnostic("command", "status requested");
                let state = if session.is_some() {
                    PrivateAccessState::Connected
                } else {
                    PrivateAccessState::Disconnected
                };
                sink.state(state, "status requested")?;
            }
            PrivateAccessCommand::AuthReply { .. } | PrivateAccessCommand::CancelAuth { .. } => {
                emit_service_error(
                    &sink,
                    "unsupported_auth_command",
                    "Hillstone service does not accept interactive authentication commands",
                )?;
            }
        }
    }
    if let Some(session) = session.take() {
        if !detached.load(Ordering::SeqCst) {
            session.shutdown.store(true, Ordering::SeqCst);
        }
        let _ = session.worker.join();
    }
    append_hillstone_diagnostic("service", "Hillstone Private Access service stopped");
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
    let credential_source = if config
        .password
        .as_ref()
        .is_some_and(|password| !password.is_empty())
    {
        "inline"
    } else {
        "environment"
    };
    append_hillstone_diagnostic(
        "session",
        &format!(
            "starting mode={} gateway={}:{} tls_verify={} timeout_secs={} credential_source={} tun_helper={} bridge_listen={}",
            config.mode.label(),
            config.server,
            config.port,
            config.tls_verify,
            config.timeout_secs,
            credential_source,
            if config.tun_helper.is_some() {
                "configured"
            } else {
                "default"
            },
            if config.mode == HillstoneServiceMode::Bridge {
                config.bridge_listen.as_str()
            } else {
                "not-applicable"
            }
        ),
    );
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
                shutdown: Some(Arc::clone(&worker_shutdown)),
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
                shutdown: Some(Arc::clone(&worker_shutdown)),
            }),
        };
        match result {
            Ok(()) => {
                append_hillstone_diagnostic(
                    "session",
                    if worker_shutdown.load(Ordering::SeqCst) {
                        "session ended after a local shutdown request"
                    } else {
                        "session ended normally"
                    },
                );
            }
            Err(error) => {
                let _ = worker_sink.error("session_failed", &format!("{error:#}"));
                let _ = worker_sink.state(PrivateAccessState::Error, "service session failed");
            }
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
        if !cfg!(test) && self.service == "hillstone" {
            append_hillstone_event_diagnostic(&event);
        }
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

fn append_hillstone_event_diagnostic(event: &PrivateAccessEvent) {
    match event {
        PrivateAccessEvent::StateChanged { state, message, .. } => append_hillstone_diagnostic(
            "state",
            &format!("state={}; message={message}", state.label()),
        ),
        PrivateAccessEvent::RoutesPushed {
            routes,
            dns,
            bridge,
            ..
        } => append_hillstone_diagnostic(
            "network",
            &format!(
                "routes_pushed routes={} dns={} bridge={}",
                routes.len(),
                dns.len(),
                bridge
                    .as_ref()
                    .map(|bridge| bridge.listen.as_str())
                    .unwrap_or("none")
            ),
        ),
        PrivateAccessEvent::Error { code, message, .. } => {
            append_hillstone_diagnostic("error", &format!("code={code}; message={message}"));
        }
        PrivateAccessEvent::Log { message, .. } => {
            append_hillstone_diagnostic("runtime", message);
        }
        PrivateAccessEvent::AuthChallenge { .. } => {
            // Never persist authentication challenge contents or replies.
        }
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
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
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
    use std::collections::HashMap;
    use std::io::{self, Write};
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{
        ExternalPrivateAccessService, HillstoneServiceConfig, HillstoneServiceMode,
        JsonLineEventSink, PrivateAccessAuthField, PrivateAccessCommand, PrivateAccessEvent,
        PrivateAccessEventEnvelope, PrivateAccessRoute, PrivateAccessSecret,
        PrivateAccessServiceCapabilities, PrivateAccessServiceManifest, PrivateAccessState,
        SONICWALL_HAPPY_EYEBALLS_DELAY, SonicwallControllerProxy, SonicwallGatewayProfileCache,
        SonicwallTransport, default_hillstone_manifest, default_sonicwall_manifest,
        describe_ipv4_packet, extract_ipv4_cidrs, ipv4_range_to_cidrs,
        is_sonicwall_team_auth_error, is_sonicwall_transport_disconnect, normalize_domain,
        normalize_ipv4_cidr, normalize_sonicwall_gateway_cache_key, normalize_tun_helper_command,
        sonicwall_candidate_delays, sonicwall_outbound_chain,
    };
    use crate::sonicwall::SonicwallLogonCapability;
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
    fn sonicwall_gateway_profile_cache_is_normalized_and_gateway_scoped() {
        let mut cache = SonicwallGatewayProfileCache::default();
        cache.update_transport(" SSLVPN.HUNDSUN.COM/ ", SonicwallTransport::Proxy);
        cache.update_logon_capability("sslvpn.hundsun.com", SonicwallLogonCapability::LegacyAdd);

        let hundsun = cache.get("sslvpn.hundsun.com/");
        assert_eq!(hundsun.transport, SonicwallTransport::Proxy);
        assert_eq!(
            hundsun.logon_capability,
            SonicwallLogonCapability::LegacyAdd
        );
        assert_eq!(
            cache.get("vpn.example.com").transport,
            SonicwallTransport::Direct
        );
        assert_eq!(
            normalize_sonicwall_gateway_cache_key(" HTTPS://VPN.Example.com/// "),
            "https://vpn.example.com"
        );
    }

    #[test]
    fn sonicwall_happy_eyeballs_gives_cached_transport_a_head_start() {
        assert_eq!(
            sonicwall_candidate_delays(SonicwallTransport::Direct),
            (Duration::ZERO, SONICWALL_HAPPY_EYEBALLS_DELAY)
        );
        assert_eq!(
            sonicwall_candidate_delays(SonicwallTransport::Proxy),
            (SONICWALL_HAPPY_EYEBALLS_DELAY, Duration::ZERO)
        );
    }

    #[test]
    fn sonicwall_gateway_profile_cache_survives_service_restart() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("sing-box-tui-sonicwall-profile-cache-{suffix}"));
        let path = directory.join("gateway-profiles.json");
        let mut cache = SonicwallGatewayProfileCache::with_path(path.clone());
        cache.update_transport("sslvpn.hundsun.com", SonicwallTransport::Proxy);
        cache.update_logon_capability("sslvpn.hundsun.com", SonicwallLogonCapability::LegacyAdd);
        cache.persist().expect("profile cache persists");

        let loaded = SonicwallGatewayProfileCache::load(path).expect("profile cache reloads");
        let profile = loaded.get("SSLVPN.HUNDSUN.COM/");
        assert_eq!(profile.transport, SonicwallTransport::Proxy);
        assert_eq!(
            profile.logon_capability,
            SonicwallLogonCapability::LegacyAdd
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn sonicwall_resource_text_extracts_only_valid_ipv4_cidrs() {
        let routes =
            extract_ipv4_cidrs(b"resource=10.22.0.0/16, 192.168.50.7/24; invalid=10.1.1.1/77");
        assert_eq!(routes, vec!["10.22.0.0/16", "192.168.50.7/24"]);
        assert_eq!(
            normalize_ipv4_cidr("172.16.0.0/12").as_deref(),
            Some("172.16.0.0/12")
        );
        assert_eq!(normalize_ipv4_cidr("172.16.0.0/33"), None);
    }

    #[test]
    fn sonicwall_range_resources_expand_to_minimal_cidrs() {
        assert_eq!(
            extract_ipv4_cidrs(b"RANGE=10.19.0.0,10.20.255.255\0"),
            vec!["10.19.0.0/16", "10.20.0.0/16"]
        );
        assert_eq!(
            extract_ipv4_cidrs(b"RANGE=10.28.6.0,10.28.7.255"),
            vec!["10.28.6.0/23"]
        );
    }

    #[test]
    fn ipv4_range_conversion_handles_unaligned_single_and_invalid_ranges() {
        assert_eq!(
            ipv4_range_to_cidrs(
                "10.30.0.1".parse().expect("start parses"),
                "10.30.0.6".parse().expect("end parses"),
            ),
            vec![
                "10.30.0.1/32",
                "10.30.0.2/31",
                "10.30.0.4/31",
                "10.30.0.6/32"
            ]
        );
        assert_eq!(
            ipv4_range_to_cidrs(
                "192.168.75.64".parse().expect("start parses"),
                "192.168.75.64".parse().expect("end parses"),
            ),
            vec!["192.168.75.64/32"]
        );
        assert!(
            ipv4_range_to_cidrs(
                "10.0.0.2".parse().expect("start parses"),
                "10.0.0.1".parse().expect("end parses"),
            )
            .is_empty()
        );
    }

    #[test]
    fn sonicwall_domain_normalization_accepts_dns_names_and_rejects_ip_literals() {
        assert_eq!(
            normalize_domain("*.Hundsun.COM.\0").as_deref(),
            Some("hundsun.com")
        );
        assert_eq!(normalize_domain("192.168.75.64"), None);
        assert_eq!(normalize_domain("bad_label.hundsun.com"), None);
        assert_eq!(normalize_domain("-bad.hundsun.com"), None);
    }

    #[test]
    fn sonicwall_packet_diagnostic_reports_ipv4_endpoints() {
        let mut packet = vec![0_u8; 20];
        packet[0] = 0x45;
        packet[9] = 6;
        packet[12..16].copy_from_slice(&[10, 22, 28, 34]);
        packet[16..20].copy_from_slice(&[10, 17, 0, 8]);
        assert_eq!(
            describe_ipv4_packet(&packet),
            "len=20 protocol=6 source=10.22.28.34 destination=10.17.0.8"
        );
    }

    #[test]
    fn sonicwall_reconnects_only_transport_level_disconnects() {
        assert!(is_sonicwall_transport_disconnect(&anyhow::anyhow!(
            "failed to read SonicWall EVPN data tunnel: connection reset by peer (os error 10054)"
        )));
        assert!(is_sonicwall_transport_disconnect(&anyhow::anyhow!(
            "SonicWall EVPN gateway closed the data tunnel"
        )));
        assert!(!is_sonicwall_transport_disconnect(&anyhow::anyhow!(
            "SonicWall EVPN gateway terminated the data tunnel"
        )));
        assert!(!is_sonicwall_transport_disconnect(&anyhow::anyhow!(
            "failed to decode SonicWall EVPN DATA packet"
        )));
    }

    #[test]
    fn sonicwall_team_auth_rejection_requires_fresh_authentication() {
        assert!(is_sonicwall_team_auth_error(&anyhow::anyhow!(
            "gateway rejected tunnel bootstrap: TEAM AUTH error"
        )));
        assert!(!is_sonicwall_team_auth_error(&anyhow::anyhow!(
            "gateway closed the data tunnel"
        )));
    }

    #[test]
    fn sonicwall_live_outbound_context_follows_selector_chain() {
        let proxies = HashMap::from([
            (
                "manual".to_string(),
                SonicwallControllerProxy {
                    now: Some("provider".to_string()),
                },
            ),
            (
                "provider".to_string(),
                SonicwallControllerProxy {
                    now: Some("node-a".to_string()),
                },
            ),
            ("node-a".to_string(), SonicwallControllerProxy { now: None }),
        ]);
        assert_eq!(
            sonicwall_outbound_chain(&proxies, "manual").as_deref(),
            Some("manual -> provider -> node-a")
        );
        assert_eq!(sonicwall_outbound_chain(&proxies, "missing"), None);
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
    fn private_access_auth_reply_serializes_without_debug_exposure() {
        let command = PrivateAccessCommand::AuthReply {
            id: "cmd-auth-1".to_string(),
            service: "sonicwall".to_string(),
            session_id: "session-1".to_string(),
            challenge_id: "challenge-1".to_string(),
            button: "ok".to_string(),
            replies: vec![PrivateAccessSecret::new("one-time-secret")],
        };

        let value = serde_json::to_value(command).expect("command serializes");
        assert_eq!(value["type"], "auth_reply");
        assert_eq!(value["replies"][0], "one-time-secret");

        let secret = PrivateAccessSecret::new("do-not-log-this");
        assert_eq!(format!("{secret:?}"), "PrivateAccessSecret([REDACTED])");
    }

    #[test]
    fn private_access_auth_challenge_matches_protocol_shape() {
        let event = PrivateAccessEventEnvelope::new(PrivateAccessEvent::AuthChallenge {
            service: "sonicwall".to_string(),
            session_id: "session-1".to_string(),
            challenge_id: "challenge-1".to_string(),
            title: "Sign in".to_string(),
            message: String::new(),
            fields: vec![PrivateAccessAuthField {
                id: "reply-0".to_string(),
                label: "Dynamic code".to_string(),
                kind: "password".to_string(),
                sensitive: true,
                required: true,
                options: Vec::new(),
            }],
            buttons: vec!["ok".to_string(), "cancel".to_string()],
        });

        let value = serde_json::to_value(event).expect("event serializes");
        assert_eq!(value["event"], "auth_challenge");
        assert_eq!(value["fields"][0]["sensitive"], true);
        assert_eq!(value["buttons"][0], "ok");
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
            domains: vec!["service.hundsun.com".to_string()],
            domain_suffixes: vec!["hundsun.com".to_string()],
            bridge: None,
        });

        let value = serde_json::to_value(event).expect("event serializes");
        assert_eq!(value["type"], "event");
        assert_eq!(value["event"], "routes_pushed");
        assert_eq!(value["routes"][0]["cidr"], "10.1.0.0/16");
        assert_eq!(value["domains"][0], "service.hundsun.com");
        assert_eq!(value["domain_suffixes"][0], "hundsun.com");
    }

    #[test]
    fn routes_pushed_event_accepts_legacy_payload_without_domains() {
        let event: PrivateAccessEventEnvelope = serde_json::from_value(json!({
            "type": "event",
            "event": "routes_pushed",
            "service": "hillstone",
            "session_id": null,
            "routes": [{ "cidr": "10.1.0.0/16" }],
            "dns": [],
            "bridge": null
        }))
        .expect("legacy routes event parses");
        match event.event {
            PrivateAccessEvent::RoutesPushed {
                domains,
                domain_suffixes,
                ..
            } => {
                assert!(domains.is_empty());
                assert!(domain_suffixes.is_empty());
            }
            _ => panic!("expected routes_pushed event"),
        }
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
    fn built_in_sonicwall_manifest_uses_clean_room_service_subcommand() {
        let manifest = default_sonicwall_manifest().expect("manifest builds");
        assert_eq!(manifest.id, "sonicwall");
        assert_eq!(manifest.protocol, "sonicwall-sma1000-evpn");
        assert_eq!(manifest.args[0], "private-access-service");
        assert_eq!(manifest.args[1], "sonicwall");
        assert_eq!(manifest.config_schema["realm"]["default"], "Hundsun");
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
