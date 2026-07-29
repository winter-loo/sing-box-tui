use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{IpAddr, Ipv4Addr};
#[cfg(target_os = "macos")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

const TUN_HELPER_READY_TIMEOUT: Duration = Duration::from_secs(180);
const TUN_HELPER_RECONFIGURE_TIMEOUT: Duration = Duration::from_secs(5);
const TUN_NON_IPV4_LOG_INTERVAL: Duration = Duration::from_secs(60);
const TUN_READ_POLL_MIN_INTERVAL: Duration = Duration::from_millis(1);
const TUN_READ_POLL_MAX_INTERVAL: Duration = Duration::from_millis(5);
const TUN_WRITE_RETRY_INTERVAL: Duration = Duration::from_millis(1);
const TUN_WRITE_WOULD_BLOCK_RETRIES: usize = 50;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TunHelperStartConfig {
    pub(crate) client_ipv4: Ipv4Addr,
    #[serde(default)]
    pub(crate) gateway_ipv4: Option<Ipv4Addr>,
    pub(crate) prefix_len: u32,
    pub(crate) route_cidrs: Vec<String>,
    #[serde(default)]
    pub(crate) dns_servers: Vec<IpAddr>,
    #[serde(default)]
    pub(crate) dns_namespaces: Vec<String>,
    #[serde(default)]
    pub(crate) dns_system_namespaces: Vec<String>,
    #[serde(default)]
    pub(crate) mtu: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TunHelperCommand {
    Start { config: TunHelperStartConfig },
    Packet { payload: String },
    Reset,
    Stop,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TunHelperEvent {
    Ready { interface: String },
    Packet { payload: String },
    Stopped { message: String },
    Error { message: String },
    Log { message: String },
}

pub(crate) struct TunHelperClient {
    child: Child,
    stdin: ChildStdin,
    event_rx: Receiver<Result<TunHelperEvent, String>>,
    stdout_worker: Option<JoinHandle<()>>,
    stderr_worker: Option<JoinHandle<()>>,
    interface: String,
}

impl TunHelperClient {
    pub(crate) fn spawn(
        command: Option<Vec<String>>,
        config: TunHelperStartConfig,
    ) -> Result<Self> {
        let command_is_explicit = command.is_some();
        let command = command.unwrap_or_else(default_tun_helper_command);
        if command.is_empty() {
            bail!("TUN helper command cannot be empty");
        }
        if should_preflight_tun_helper_command(command_is_explicit, &command) {
            preflight_tun_helper_command(&command)?;
        }
        let mut process = Command::new(&command[0]);
        process.args(&command[1..]);
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = process.spawn().with_context(|| {
            format!(
                "failed to spawn Private Access TUN helper command: {}",
                command.join(" ")
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .context("TUN helper stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("TUN helper stdout was not piped")?;
        let stderr = child
            .stderr
            .take()
            .context("TUN helper stderr was not piped")?;
        let (tx, rx) = mpsc::channel();
        let stdout_tx = tx.clone();
        let stdout_worker = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        let event =
                            serde_json::from_str::<TunHelperEvent>(&line).map_err(|error| {
                                format!(
                                    "failed to parse TUN helper event JSON: {error}; line={line}"
                                )
                            });
                        let _ = stdout_tx.send(event);
                    }
                    Err(error) => {
                        let _ = stdout_tx
                            .send(Err(format!("failed to read TUN helper stdout: {error}")));
                        break;
                    }
                }
            }
        });
        let stderr_tx = tx;
        let stderr_worker = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) if line.trim().is_empty() => {}
                    Ok(line) => {
                        let _ = stderr_tx.send(Ok(TunHelperEvent::Log { message: line }));
                    }
                    Err(error) => {
                        let _ = stderr_tx
                            .send(Err(format!("failed to read TUN helper stderr: {error}")));
                        break;
                    }
                }
            }
        });

        let mut client = Self {
            child,
            stdin,
            event_rx: rx,
            stdout_worker: Some(stdout_worker),
            stderr_worker: Some(stderr_worker),
            interface: String::new(),
        };
        client.send_command(&TunHelperCommand::Start { config })?;
        client.wait_ready(TUN_HELPER_READY_TIMEOUT)?;
        Ok(client)
    }

    pub(crate) fn interface(&self) -> &str {
        &self.interface
    }

    pub(crate) fn send_ipv4(&mut self, packet: &[u8]) -> Result<bool> {
        if packet.first().map(|byte| byte >> 4) != Some(4) {
            bail!("TUN helper write requires an inner IPv4 packet");
        }
        self.send_command(&TunHelperCommand::Packet {
            payload: BASE64.encode(packet),
        })?;
        Ok(true)
    }

    pub(crate) fn try_recv_ipv4(&self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.event_rx.try_recv() {
                Ok(Ok(TunHelperEvent::Packet { payload })) => {
                    let packet = BASE64
                        .decode(payload)
                        .context("failed to decode TUN helper packet payload")?;
                    if packet.first().map(|byte| byte >> 4) != Some(4) {
                        bail!("TUN helper emitted a non-IPv4 packet");
                    }
                    return Ok(Some(packet));
                }
                Ok(Ok(TunHelperEvent::Log { message })) => {
                    eprintln!("TUN helper: {message}");
                }
                Ok(Ok(TunHelperEvent::Stopped { message })) => {
                    bail!("TUN helper stopped: {message}");
                }
                Ok(Ok(TunHelperEvent::Error { message })) => {
                    bail!("TUN helper failed: {message}");
                }
                Ok(Ok(TunHelperEvent::Ready { .. })) => {}
                Ok(Err(error)) => bail!("{error}"),
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => bail!("TUN helper event stream closed"),
            }
        }
    }

    pub(crate) fn start_session(&mut self, config: TunHelperStartConfig) -> Result<()> {
        self.send_command(&TunHelperCommand::Start { config })?;
        self.wait_ready(TUN_HELPER_RECONFIGURE_TIMEOUT)
    }

    pub(crate) fn reset_session(&mut self) -> Result<()> {
        self.send_command(&TunHelperCommand::Reset)?;
        self.wait_stopped()?;
        self.interface.clear();
        Ok(())
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        let _ = self.send_command(&TunHelperCommand::Stop);
        let _ = self.child.wait();
        if let Some(worker) = self.stdout_worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.stderr_worker.take() {
            let _ = worker.join();
        }
        Ok(())
    }

    fn send_command(&mut self, command: &TunHelperCommand) -> Result<()> {
        let line = serde_json::to_string(command).context("failed to encode TUN helper command")?;
        writeln!(self.stdin, "{line}").context("failed to write TUN helper command")?;
        self.stdin
            .flush()
            .context("failed to flush TUN helper command")?;
        Ok(())
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        let mut recent_logs = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for TUN helper to become ready{}",
                    helper_log_suffix(&recent_logs)
                );
            }
            match self
                .event_rx
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(Ok(TunHelperEvent::Ready { interface })) => {
                    self.interface = interface;
                    return Ok(());
                }
                Ok(Ok(TunHelperEvent::Log { message })) => {
                    remember_helper_log(&mut recent_logs, &message);
                    eprintln!("TUN helper: {message}");
                }
                Ok(Ok(TunHelperEvent::Error { message })) => {
                    bail!(
                        "TUN helper failed before ready: {message}{}",
                        helper_log_suffix(&recent_logs)
                    )
                }
                Ok(Ok(TunHelperEvent::Stopped { message })) => {
                    bail!(
                        "TUN helper stopped before ready: {message}{}",
                        helper_log_suffix(&recent_logs)
                    )
                }
                Ok(Ok(TunHelperEvent::Packet { .. })) => {}
                Ok(Err(error)) => bail!("{error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = self
                        .child
                        .try_wait()
                        .context("failed to inspect TUN helper process status")?
                    {
                        bail!(
                            "TUN helper exited before ready with status {status}{}",
                            helper_log_suffix(&recent_logs)
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!(
                        "TUN helper event stream closed before ready{}",
                        helper_log_suffix(&recent_logs)
                    )
                }
            }
        }
    }

    fn wait_stopped(&mut self) -> Result<()> {
        let deadline = Instant::now() + TUN_HELPER_RECONFIGURE_TIMEOUT;
        let mut recent_logs = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for TUN helper session to reset{}",
                    helper_log_suffix(&recent_logs)
                );
            }
            match self
                .event_rx
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(Ok(TunHelperEvent::Stopped { .. })) => return Ok(()),
                Ok(Ok(TunHelperEvent::Log { message })) => {
                    remember_helper_log(&mut recent_logs, &message);
                    eprintln!("TUN helper: {message}");
                }
                Ok(Ok(TunHelperEvent::Error { message })) => {
                    bail!(
                        "TUN helper failed while resetting: {message}{}",
                        helper_log_suffix(&recent_logs)
                    )
                }
                Ok(Ok(TunHelperEvent::Packet { .. } | TunHelperEvent::Ready { .. })) => {}
                Ok(Err(error)) => bail!("{error}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = self
                        .child
                        .try_wait()
                        .context("failed to inspect TUN helper process status")?
                    {
                        bail!(
                            "TUN helper exited while resetting with status {status}{}",
                            helper_log_suffix(&recent_logs)
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!(
                        "TUN helper event stream closed while resetting{}",
                        helper_log_suffix(&recent_logs)
                    )
                }
            }
        }
    }
}

fn preflight_tun_helper_command(command: &[String]) -> Result<()> {
    if !is_noninteractive_sudo_command(command) {
        return Ok(());
    }
    let output = Command::new("sudo")
        .args(["-n", "true"])
        .output()
        .context("failed to check non-interactive sudo for TUN helper")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    bail!(
        "TUN mode needs sudo authorization before starting the helper; run `sudo -v` in a terminal and retry{}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(" ({detail})")
        }
    );
}

fn is_noninteractive_sudo_command(command: &[String]) -> bool {
    command.first().is_some_and(|program| program == "sudo")
        && command.iter().skip(1).any(|arg| arg == "-n")
}

fn should_preflight_tun_helper_command(command_is_explicit: bool, command: &[String]) -> bool {
    !command_is_explicit && is_noninteractive_sudo_command(command)
}

fn remember_helper_log(recent_logs: &mut Vec<String>, message: &str) {
    recent_logs.push(message.to_string());
    if recent_logs.len() > 3 {
        recent_logs.remove(0);
    }
}

fn helper_log_suffix(recent_logs: &[String]) -> String {
    if recent_logs.is_empty() {
        String::new()
    } else {
        format!("; helper output: {}", recent_logs.join(" | "))
    }
}

impl Drop for TunHelperClient {
    fn drop(&mut self) {
        let _ = self.send_command(&TunHelperCommand::Stop);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(worker) = self.stdout_worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.stderr_worker.take() {
            let _ = worker.join();
        }
    }
}

fn default_tun_helper_command() -> Vec<String> {
    let exe = std::env::current_exe()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| "sing-box-tui".to_string());
    let helper_args = vec![
        exe,
        "private-access-tun-helper".to_string(),
        "--stdio".to_string(),
    ];
    if tun_helper_can_run_directly() {
        helper_args
    } else {
        let mut command = vec!["sudo".to_string(), "-n".to_string()];
        command.extend(helper_args);
        command
    }
}

#[cfg(unix)]
fn tun_helper_can_run_directly() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn tun_helper_can_run_directly() -> bool {
    true
}

pub(crate) fn run_private_access_tun_helper_stdio(stdio: bool) -> Result<()> {
    if !stdio {
        bail!("private-access-tun-helper currently requires --stdio");
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let mut context: Option<TunHelperContext> = None;
    for line in std::io::stdin().lock().lines() {
        let line = line.context("failed to read TUN helper command")?;
        if line.trim().is_empty() {
            continue;
        }
        let command: TunHelperCommand =
            serde_json::from_str(&line).context("failed to parse TUN helper command JSON")?;
        match command {
            TunHelperCommand::Start { config } => {
                if context.is_some() {
                    emit_tun_helper_event(
                        &stdout,
                        &TunHelperEvent::Error {
                            message: "TUN helper session already exists".to_string(),
                        },
                    )?;
                    continue;
                }
                match TunHelperContext::start(config, Arc::clone(&stdout)) {
                    Ok(next) => {
                        emit_tun_helper_event(
                            &stdout,
                            &TunHelperEvent::Ready {
                                interface: next.interface.clone(),
                            },
                        )?;
                        context = Some(next);
                    }
                    Err(error) => {
                        emit_tun_helper_event(
                            &stdout,
                            &TunHelperEvent::Error {
                                message: format!("{error:#}"),
                            },
                        )?;
                    }
                }
            }
            TunHelperCommand::Packet { payload } => {
                let Some(context) = context.as_mut() else {
                    emit_tun_helper_event(
                        &stdout,
                        &TunHelperEvent::Error {
                            message: "TUN helper has no active session".to_string(),
                        },
                    )?;
                    continue;
                };
                let packet = BASE64
                    .decode(payload)
                    .context("failed to decode TUN helper packet command")?;
                if let Err(error) = context.write_ipv4(&packet) {
                    emit_tun_helper_event(
                        &stdout,
                        &TunHelperEvent::Error {
                            message: format!("{error:#}"),
                        },
                    )?;
                }
            }
            TunHelperCommand::Reset => {
                if let Some(context) = context.take() {
                    context.stop();
                }
                emit_tun_helper_event(
                    &stdout,
                    &TunHelperEvent::Stopped {
                        message: "session reset by request".to_string(),
                    },
                )?;
            }
            TunHelperCommand::Stop => {
                if let Some(context) = context.take() {
                    context.stop();
                }
                emit_tun_helper_event(
                    &stdout,
                    &TunHelperEvent::Stopped {
                        message: "stopped by request".to_string(),
                    },
                )?;
                break;
            }
        }
    }
    if let Some(context) = context.take() {
        context.stop();
    }
    Ok(())
}

fn emit_tun_helper_event(
    stdout: &Arc<Mutex<std::io::Stdout>>,
    event: &TunHelperEvent,
) -> Result<()> {
    let line = serde_json::to_string(event).context("failed to encode TUN helper event")?;
    let mut writer = stdout
        .lock()
        .map_err(|_| anyhow::anyhow!("TUN helper stdout mutex poisoned"))?;
    writeln!(writer, "{line}").context("failed to write TUN helper event")?;
    writer.flush().context("failed to flush TUN helper event")?;
    Ok(())
}

#[cfg(any(
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
))]
struct TunHelperContext {
    device: Arc<tun_rs::SyncDevice>,
    interface: String,
    _dns_policy: TunDnsPolicyGuard,
    _routes: TunRouteGuard,
    shutdown: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

#[cfg(any(
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
))]
impl TunHelperContext {
    fn start(config: TunHelperStartConfig, stdout: Arc<Mutex<std::io::Stdout>>) -> Result<Self> {
        if config.prefix_len > 32 {
            bail!(
                "invalid IPv4 prefix length for TUN interface: {}",
                config.prefix_len
            );
        }
        // The service process owns Hillstone session keys; the helper owns only the privileged
        // kernel-facing TUN state. This split was added after direct service-side TUN setup made
        // the normal TUI binary require root privileges.
        let mut builder = tun_rs::DeviceBuilder::new().ipv4(
            config.client_ipv4,
            config.prefix_len as u8,
            config.gateway_ipv4,
        );
        if let Some(mtu) = config.mtu {
            #[cfg(windows)]
            {
                builder = builder.mtu_v4(mtu);
            }
            #[cfg(not(windows))]
            {
                builder = builder.mtu(mtu);
            }
        }
        let device = builder
            .with(|_builder| {
                #[cfg(any(
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd"
                ))]
                _builder.associate_route(false);
                #[cfg(any(
                    target_os = "macos",
                    target_os = "linux",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd"
                ))]
                _builder.packet_information(false);
            })
            .build_sync()
            .context("failed to create TUN device with tun-rs")?;
        #[cfg(windows)]
        if !config.dns_servers.is_empty() && config.dns_namespaces.is_empty() {
            device
                .set_dns_servers(&config.dns_servers)
                .context("failed to apply pushed DNS servers to TUN interface")?;
        }
        #[cfg(unix)]
        device
            .set_nonblocking(true)
            .context("failed to set TUN helper device nonblocking")?;
        let interface = device.name().context("failed to read TUN interface name")?;
        let route_cidrs =
            tun_route_cidrs_with_dns_servers(&config.route_cidrs, &config.dns_servers)?;
        let routes = TunRouteGuard::install_routes(&interface, &route_cidrs)
            .context("failed to install pushed TUN routes")?;
        let dns_policy = TunDnsPolicyGuard::install(
            &interface,
            &config.dns_namespaces,
            &config.dns_system_namespaces,
            &config.dns_servers,
        )
        .context("failed to install Private Access split-DNS policy")?;
        let device = Arc::new(device);
        let reader_device = Arc::clone(&device);
        let shutdown = Arc::new(AtomicBool::new(false));
        let reader_shutdown = Arc::clone(&shutdown);
        let reader = thread::spawn(move || {
            let mut buffer = vec![0_u8; 65535];
            let mut dropped_non_ipv4 = 0_u64;
            let mut next_non_ipv4_log = Instant::now();
            let mut read_poll_interval = TUN_READ_POLL_MIN_INTERVAL;
            while !reader_shutdown.load(Ordering::SeqCst) {
                #[cfg(windows)]
                let receive = reader_device.try_recv(&mut buffer);
                #[cfg(not(windows))]
                let receive = reader_device.recv(&mut buffer);
                match receive {
                    Ok(size) => {
                        read_poll_interval = TUN_READ_POLL_MIN_INTERVAL;
                        let packet = &buffer[..size];
                        if packet.first().map(|byte| byte >> 4) != Some(4) {
                            dropped_non_ipv4 = dropped_non_ipv4.saturating_add(1);
                            let now = Instant::now();
                            if now >= next_non_ipv4_log {
                                let _ = emit_tun_helper_event(
                                    &stdout,
                                    &TunHelperEvent::Log {
                                        message: format!(
                                            "dropped {dropped_non_ipv4} non-IPv4 packet(s) from TUN since previous report"
                                        ),
                                    },
                                );
                                dropped_non_ipv4 = 0;
                                next_non_ipv4_log = now + TUN_NON_IPV4_LOG_INTERVAL;
                            }
                            continue;
                        }
                        let _ = emit_tun_helper_event(
                            &stdout,
                            &TunHelperEvent::Packet {
                                payload: BASE64.encode(packet),
                            },
                        );
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(read_poll_interval);
                        read_poll_interval = (read_poll_interval + TUN_READ_POLL_MIN_INTERVAL)
                            .min(TUN_READ_POLL_MAX_INTERVAL);
                    }
                    Err(error) => {
                        let _ = emit_tun_helper_event(
                            &stdout,
                            &TunHelperEvent::Error {
                                message: format!("failed to read from TUN: {error}"),
                            },
                        );
                        break;
                    }
                }
            }
        });
        Ok(Self {
            device,
            interface,
            _dns_policy: dns_policy,
            _routes: routes,
            shutdown,
            reader: Some(reader),
        })
    }

    fn write_ipv4(&self, packet: &[u8]) -> Result<()> {
        if packet.first().map(|byte| byte >> 4) != Some(4) {
            bail!("TUN helper write requires an inner IPv4 packet");
        }
        write_ipv4_with_retry(packet.len(), || self.device.send(packet))
    }

    fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        let _ = self.device.shutdown();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(any(
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
))]
fn write_ipv4_with_retry<F>(packet_len: usize, mut send: F) -> Result<()>
where
    F: FnMut() -> std::io::Result<usize>,
{
    let mut would_block_retries = 0;
    loop {
        match send() {
            Ok(size) if size == packet_len => return Ok(()),
            Ok(size) => bail!(
                "short TUN helper write: wrote {size} of {} bytes",
                packet_len
            ),
            Err(error)
                if error.kind() == ErrorKind::WouldBlock
                    && would_block_retries < TUN_WRITE_WOULD_BLOCK_RETRIES =>
            {
                would_block_retries += 1;
                thread::sleep(TUN_WRITE_RETRY_INTERVAL);
                continue;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => bail!(
                "timed out writing an IPv4 packet to TUN after {} retries",
                TUN_WRITE_WOULD_BLOCK_RETRIES
            ),
            Err(error) => {
                return Err(error).context("failed to write IPv4 packet to TUN");
            }
        }
    }
}

#[cfg(not(any(
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
)))]
struct TunHelperContext;

#[cfg(not(any(
    target_os = "windows",
    all(target_os = "linux", not(target_env = "ohos")),
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
)))]
impl TunHelperContext {
    fn start(_config: TunHelperStartConfig, _stdout: Arc<Mutex<std::io::Stdout>>) -> Result<Self> {
        bail!("Private Access TUN helper is not supported on this platform")
    }

    fn write_ipv4(&self, _packet: &[u8]) -> Result<()> {
        bail!("Private Access TUN helper is not supported on this platform")
    }

    fn stop(self) {}
}

fn tun_route_cidrs_with_dns_servers(
    route_cidrs: &[String],
    dns_servers: &[IpAddr],
) -> Result<Vec<String>> {
    let mut routes = Vec::new();
    let mut parsed_routes = Vec::new();
    let mut seen = BTreeSet::new();
    for cidr in route_cidrs {
        let (network, prefix_len) = parse_ipv4_cidr(cidr)
            .with_context(|| format!("invalid TUN route CIDR pushed by Private Access: {cidr}"))?;
        let normalized = format!("{network}/{prefix_len}");
        if seen.insert(normalized.clone()) {
            routes.push(normalized);
            parsed_routes.push((network, prefix_len));
        }
    }
    for server in dns_servers {
        let IpAddr::V4(server) = server else {
            continue;
        };
        if parsed_routes
            .iter()
            .any(|(network, prefix_len)| ipv4_cidr_contains(*network, *prefix_len, *server))
        {
            continue;
        }
        let host_route = format!("{server}/32");
        if seen.insert(host_route.clone()) {
            routes.push(host_route);
            parsed_routes.push((*server, 32));
        }
    }
    Ok(routes)
}

fn ipv4_cidr_contains(network: Ipv4Addr, prefix_len: u8, address: Ipv4Addr) -> bool {
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    };
    u32::from(network) & mask == u32::from(address) & mask
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
fn normalize_dns_namespace(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    let is_suffix = value.starts_with("*.") || value.starts_with('.');
    let domain = value
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .trim_end_matches('.');
    if domain.is_empty()
        || domain.len() > 253
        || domain.parse::<IpAddr>().is_ok()
        || !domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return None;
    }
    Some(if is_suffix {
        format!(".{domain}")
    } else {
        domain.to_string()
    })
}

#[cfg(any(windows, target_os = "macos", test))]
#[derive(Debug, Eq, PartialEq)]
struct TunDnsPolicyPlan {
    tunnel_namespaces: Vec<String>,
    system_namespaces: Vec<String>,
}

#[cfg(any(windows, target_os = "macos", test))]
fn dns_namespace_matches_domain(namespace: &str, domain: &str) -> bool {
    namespace
        .strip_prefix('.')
        .is_some_and(|suffix| domain == suffix || domain.ends_with(&format!(".{suffix}")))
        || namespace == domain
}

#[cfg(any(windows, target_os = "macos", test))]
fn tun_dns_policy_plan(namespaces: &[String], system_namespaces: &[String]) -> TunDnsPolicyPlan {
    let mut tunnel_namespaces = namespaces
        .iter()
        .filter_map(|namespace| normalize_dns_namespace(namespace))
        .collect::<Vec<_>>();
    tunnel_namespaces.sort();
    tunnel_namespaces.dedup();

    let mut requested_system_namespaces = system_namespaces
        .iter()
        .filter_map(|namespace| normalize_dns_namespace(namespace))
        .filter(|namespace| !namespace.starts_with('.'))
        .collect::<Vec<_>>();
    requested_system_namespaces.sort();
    requested_system_namespaces.dedup();

    // An exact tunnel rule for an excluded gateway would tie with the system-DNS rule. Remove it;
    // broader suffix rules remain in place for dynamic intranet hostnames and are overridden by
    // the more-specific system-DNS rule below.
    tunnel_namespaces.retain(|namespace| !requested_system_namespaces.contains(namespace));
    let system_namespaces = requested_system_namespaces
        .into_iter()
        .filter(|domain| {
            tunnel_namespaces
                .iter()
                .any(|namespace| dns_namespace_matches_domain(namespace, domain))
        })
        .collect();

    TunDnsPolicyPlan {
        tunnel_namespaces,
        system_namespaces,
    }
}

#[cfg(target_os = "windows")]
struct TunDnsPolicyGuard {
    rule_names: Vec<String>,
}

#[cfg(target_os = "windows")]
impl TunDnsPolicyGuard {
    fn install(
        _interface: &str,
        namespaces: &[String],
        system_namespaces: &[String],
        dns_servers: &[IpAddr],
    ) -> Result<Self> {
        let plan = tun_dns_policy_plan(namespaces, system_namespaces);
        if plan.tunnel_namespaces.is_empty() && plan.system_namespaces.is_empty() {
            return Ok(Self {
                rule_names: Vec::new(),
            });
        }
        let mut servers = dns_servers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        servers.sort();
        servers.dedup();
        if servers.is_empty() && !plan.tunnel_namespaces.is_empty() {
            bail!("Private Access pushed DNS namespaces without any DNS server");
        }

        let marker = format!("sing-box-tui private access pid={}", std::process::id());
        let servers = servers.join(",");
        let mut guard = Self {
            rule_names: Vec::new(),
        };
        for namespace in plan.tunnel_namespaces {
            let output = run_windows_powershell(
                WINDOWS_ADD_NRPT_RULE_SCRIPT,
                &[
                    ("SING_BOX_TUI_NRPT_NAMESPACE", namespace.as_str()),
                    ("SING_BOX_TUI_NRPT_SERVERS", servers.as_str()),
                    ("SING_BOX_TUI_NRPT_SERVER_MODE", "explicit"),
                    ("SING_BOX_TUI_NRPT_MARKER", marker.as_str()),
                ],
            )
            .with_context(|| format!("failed to add Windows NRPT rule for {namespace}"))?;
            let rule_name = output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .next_back()
                .context("Add-DnsClientNrptRule did not return a rule name")?;
            guard.rule_names.push(rule_name.to_string());
        }
        for namespace in plan.system_namespaces {
            let output = run_windows_powershell(
                WINDOWS_ADD_NRPT_RULE_SCRIPT,
                &[
                    ("SING_BOX_TUI_NRPT_NAMESPACE", namespace.as_str()),
                    ("SING_BOX_TUI_NRPT_SERVERS", ""),
                    ("SING_BOX_TUI_NRPT_SERVER_MODE", "system"),
                    ("SING_BOX_TUI_NRPT_MARKER", marker.as_str()),
                ],
            )
            .with_context(|| {
                format!("failed to add Windows system-DNS override for {namespace}")
            })?;
            let rule_name = output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .next_back()
                .context("Add-DnsClientNrptRule did not return a rule name")?;
            guard.rule_names.push(rule_name.to_string());
        }
        Ok(guard)
    }
}

#[cfg(target_os = "windows")]
impl Drop for TunDnsPolicyGuard {
    fn drop(&mut self) {
        for rule_name in self.rule_names.iter().rev() {
            if let Err(error) = run_windows_powershell(
                WINDOWS_REMOVE_NRPT_RULE_SCRIPT,
                &[("SING_BOX_TUI_NRPT_RULE_NAME", rule_name.as_str())],
            ) {
                eprintln!("warning: failed to remove Windows NRPT rule {rule_name}: {error:#}");
            }
        }
    }
}

#[cfg(target_os = "windows")]
const WINDOWS_ADD_NRPT_RULE_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$namespace = $env:SING_BOX_TUI_NRPT_NAMESPACE
$serverMode = $env:SING_BOX_TUI_NRPT_SERVER_MODE
if ($serverMode -eq 'system') {
    $servers = @()
    $allDefaultRoutes = @(
        Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' |
            Where-Object { $_.NextHop -ne '0.0.0.0' } |
            Sort-Object RouteMetric, InterfaceMetric
    )
    $physicalInterfaceIndexes = @(
        Get-NetAdapter -Physical |
            Where-Object { $_.Status -eq 'Up' } |
            ForEach-Object { $_.ifIndex }
    )
    $defaultRoutes = @(
        $allDefaultRoutes |
            Where-Object { $physicalInterfaceIndexes -contains $_.InterfaceIndex }
    )
    if ($defaultRoutes.Count -eq 0) {
        $defaultRoutes = $allDefaultRoutes
    }
    foreach ($route in $defaultRoutes) {
        $servers = @(
            (Get-DnsClientServerAddress -InterfaceIndex $route.InterfaceIndex -AddressFamily IPv4).ServerAddresses |
                Where-Object { $_ }
        )
        if ($servers.Count -gt 0) { break }
    }
    if ($servers.Count -eq 0) {
        throw "No IPv4 DNS servers found on a default-route interface for NRPT namespace: $namespace"
    }
} else {
    $servers = @($env:SING_BOX_TUI_NRPT_SERVERS.Split(',') | Where-Object { $_ })
}
$marker = $env:SING_BOX_TUI_NRPT_MARKER
$existing = @(Get-DnsClientNrptRule | Where-Object { @($_.Namespace) -contains $namespace })
foreach ($rule in $existing) {
    if ($rule.Comment -match '^sing-box-tui private access pid=(\d+)$') {
        $owner = Get-Process -Id ([int]$Matches[1]) -ErrorAction SilentlyContinue
        if ($null -eq $owner) {
            Remove-DnsClientNrptRule -Name $rule.Name -Force -Confirm:$false
            continue
        }
    }
    throw "Windows NRPT namespace already has a rule: $namespace"
}
$rule = Add-DnsClientNrptRule -Namespace $namespace -NameServers $servers -Comment $marker -DisplayName $marker -PassThru
$rule.Name
"#;

#[cfg(target_os = "windows")]
const WINDOWS_REMOVE_NRPT_RULE_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Remove-DnsClientNrptRule -Name $env:SING_BOX_TUI_NRPT_RULE_NAME -Force -Confirm:$false
"#;

#[cfg(target_os = "windows")]
fn run_windows_powershell(script: &str, environment: &[(&str, &str)]) -> Result<String> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .envs(environment.iter().copied())
        .output()
        .context("failed to start Windows PowerShell for split DNS")?;
    if !output.status.success() {
        bail!(
            "Windows PowerShell exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
struct MacosResolverFile {
    path: PathBuf,
    contents: String,
}

#[cfg(target_os = "macos")]
struct TunDnsPolicyGuard {
    resolver_files: Vec<MacosResolverFile>,
}

#[cfg(target_os = "macos")]
impl TunDnsPolicyGuard {
    fn install(
        _interface: &str,
        namespaces: &[String],
        system_namespaces: &[String],
        dns_servers: &[IpAddr],
    ) -> Result<Self> {
        let plan = tun_dns_policy_plan(namespaces, system_namespaces);
        if plan.tunnel_namespaces.is_empty() && plan.system_namespaces.is_empty() {
            return Ok(Self {
                resolver_files: Vec::new(),
            });
        }
        if dns_servers.is_empty() && !plan.tunnel_namespaces.is_empty() {
            bail!("Private Access pushed DNS namespaces without any DNS server");
        }

        let system_dns_servers = if plan.system_namespaces.is_empty() {
            Vec::new()
        } else {
            macos_default_dns_servers().context(
                "failed to find the macOS primary DNS servers for the VPN gateway override",
            )?
        };
        let resolver_dir = Path::new("/etc/resolver");
        ensure_macos_resolver_directory(resolver_dir)?;
        let pid = std::process::id();
        let mut guard = Self {
            resolver_files: Vec::new(),
        };
        for namespace in plan.tunnel_namespaces {
            guard.install_resolver(resolver_dir, &namespace, dns_servers, pid)?;
        }
        for namespace in plan.system_namespaces {
            guard.install_resolver(resolver_dir, &namespace, &system_dns_servers, pid)?;
        }
        flush_macos_dns_cache();
        Ok(guard)
    }

    fn install_resolver(
        &mut self,
        resolver_dir: &Path,
        namespace: &str,
        dns_servers: &[IpAddr],
        pid: u32,
    ) -> Result<()> {
        let domain = namespace.trim_start_matches('.');
        let contents = macos_resolver_file_contents(namespace, dns_servers, pid)?;
        let path = resolver_dir.join(domain);
        remove_stale_owned_macos_resolver(&path)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| {
                format!(
                    "failed to create macOS split-DNS resolver {}",
                    path.display()
                )
            })?;
        let write_result = file
            .write_all(contents.as_bytes())
            .with_context(|| format!("failed to write macOS resolver {}", path.display()))
            .and_then(|()| {
                file.flush()
                    .with_context(|| format!("failed to flush macOS resolver {}", path.display()))
            });
        if let Err(error) = write_result {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        self.resolver_files
            .push(MacosResolverFile { path, contents });
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for TunDnsPolicyGuard {
    fn drop(&mut self) {
        for resolver in self.resolver_files.iter().rev() {
            match fs::read_to_string(&resolver.path) {
                Ok(contents) if contents == resolver.contents => {
                    if let Err(error) = fs::remove_file(&resolver.path) {
                        eprintln!(
                            "warning: failed to remove macOS resolver {}: {error}",
                            resolver.path.display()
                        );
                    }
                }
                Ok(_) => eprintln!(
                    "warning: not removing modified macOS resolver {}",
                    resolver.path.display()
                ),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => eprintln!(
                    "warning: failed to inspect macOS resolver {} during cleanup: {error}",
                    resolver.path.display()
                ),
            }
        }
        flush_macos_dns_cache();
    }
}

#[cfg(target_os = "macos")]
fn macos_resolver_file_contents(
    namespace: &str,
    dns_servers: &[IpAddr],
    pid: u32,
) -> Result<String> {
    let domain = normalize_dns_namespace(namespace)
        .map(|namespace| namespace.trim_start_matches('.').to_string())
        .context("invalid macOS split-DNS namespace")?;
    if domain.is_empty() || dns_servers.is_empty() {
        bail!("macOS resolver {domain} requires at least one DNS server");
    }
    let mut contents = format!("# sing-box-tui private access pid={pid}\n");
    for server in dns_servers.iter().take(3) {
        contents.push_str(&format!("nameserver {server}\n"));
    }
    Ok(contents)
}

#[cfg(target_os = "macos")]
fn ensure_macos_resolver_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "macOS resolver path {} is not a real directory",
                path.display()
            )
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| {
                format!(
                    "failed to create macOS resolver directory {}",
                    path.display()
                )
            })
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to inspect macOS resolver directory {}",
                path.display()
            )
        }),
    }
}

#[cfg(target_os = "macos")]
fn remove_stale_owned_macos_resolver(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect macOS resolver {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "macOS resolver path {} is not a regular file",
            path.display()
        );
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read existing macOS resolver {}", path.display()))?;
    let owner = contents
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("# sing-box-tui private access pid="))
        .and_then(|pid| pid.parse::<u32>().ok())
        .with_context(|| {
            format!(
                "macOS split-DNS resolver already exists and is not owned by sing-box-tui: {}",
                path.display()
            )
        })?;
    if macos_process_exists(owner) {
        bail!(
            "macOS split-DNS resolver {} is owned by active process {owner}",
            path.display()
        );
    }
    fs::remove_file(path)
        .with_context(|| format!("failed to remove stale macOS resolver {}", path.display()))
}

#[cfg(target_os = "macos")]
fn macos_process_exists(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
fn macos_default_dns_servers() -> Result<Vec<IpAddr>> {
    let output = Command::new("/usr/sbin/scutil")
        .arg("--dns")
        .output()
        .context("failed to inspect macOS DNS configuration with scutil")?;
    if !output.status.success() {
        bail!("scutil --dns exited with status {}", output.status);
    }
    let servers = macos_default_dns_servers_from_scutil(&String::from_utf8_lossy(&output.stdout));
    if servers.is_empty() {
        bail!("macOS DNS configuration has no primary non-supplemental resolver");
    }
    Ok(servers)
}

#[cfg(target_os = "macos")]
fn macos_default_dns_servers_from_scutil(text: &str) -> Vec<IpAddr> {
    for block in text.split("\n\n") {
        if block.starts_with("DNS configuration (for scoped queries)") {
            break;
        }
        if !block.trim_start().starts_with("resolver #")
            || block
                .lines()
                .any(|line| line.trim_start().starts_with("domain "))
            || block.lines().any(|line| line.contains("Supplemental"))
        {
            continue;
        }
        let servers = block
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                line.starts_with("nameserver[")
                    .then(|| line.split_once(':')?.1.trim().parse::<IpAddr>().ok())
                    .flatten()
            })
            .collect::<Vec<_>>();
        if !servers.is_empty() {
            return servers;
        }
    }
    Vec::new()
}

#[cfg(target_os = "macos")]
fn flush_macos_dns_cache() {
    let _ = Command::new("/usr/bin/dscacheutil")
        .arg("-flushcache")
        .status();
    let _ = Command::new("/usr/bin/killall")
        .args(["-HUP", "mDNSResponder"])
        .status();
}

#[cfg(target_os = "linux")]
struct TunDnsPolicyGuard {
    interface: String,
    configured: bool,
}

#[cfg(target_os = "linux")]
impl TunDnsPolicyGuard {
    fn install(
        interface: &str,
        namespaces: &[String],
        _system_namespaces: &[String],
        dns_servers: &[IpAddr],
    ) -> Result<Self> {
        let mut guard = Self {
            interface: interface.to_string(),
            configured: false,
        };
        for args in linux_resolvectl_configure_args(interface, namespaces, dns_servers)? {
            run_command("resolvectl", &args)
                .context("failed to configure systemd-resolved split DNS")?;
            guard.configured = true;
        }
        Ok(guard)
    }
}

#[cfg(target_os = "linux")]
impl Drop for TunDnsPolicyGuard {
    fn drop(&mut self) {
        if !self.configured {
            return;
        }
        let args = vec!["revert".to_string(), self.interface.clone()];
        if let Err(error) = run_command("resolvectl", &args) {
            eprintln!(
                "warning: failed to revert split DNS on {}: {error:#}",
                self.interface
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_resolvectl_configure_args(
    interface: &str,
    namespaces: &[String],
    dns_servers: &[IpAddr],
) -> Result<Vec<Vec<String>>> {
    let route_domains = namespaces
        .iter()
        .filter_map(|namespace| normalize_dns_namespace(namespace))
        .map(|namespace| format!("~{}", namespace.trim_start_matches('.')))
        .collect::<BTreeSet<_>>();
    if route_domains.is_empty() {
        return Ok(Vec::new());
    }
    if dns_servers.is_empty() {
        bail!("Private Access pushed DNS namespaces without any DNS server");
    }

    let mut dns_args = vec!["dns".to_string(), interface.to_string()];
    dns_args.extend(dns_servers.iter().map(ToString::to_string));
    let mut domain_args = vec!["domain".to_string(), interface.to_string()];
    domain_args.extend(route_domains);
    Ok(vec![
        dns_args,
        domain_args,
        vec![
            "default-route".to_string(),
            interface.to_string(),
            "false".to_string(),
        ],
    ])
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
struct TunDnsPolicyGuard;

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
impl TunDnsPolicyGuard {
    fn install(
        _interface: &str,
        _namespaces: &[String],
        _system_namespaces: &[String],
        _dns_servers: &[IpAddr],
    ) -> Result<Self> {
        Ok(Self)
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct TunRouteGuard {
    interface: String,
    routes: Vec<String>,
}

#[cfg(target_os = "macos")]
impl TunRouteGuard {
    pub(crate) fn install_routes(interface: &str, route_cidrs: &[String]) -> Result<Self> {
        let mut guard = Self {
            interface: interface.to_string(),
            routes: Vec::new(),
        };
        for cidr in route_cidrs {
            let route_args = route_add_args(interface, cidr);
            run_command("route", &route_args)
                .with_context(|| format!("failed to add TUN route {cidr} via {interface}"))?;
            guard.routes.push(cidr.clone());
        }
        Ok(guard)
    }
}

#[cfg(target_os = "macos")]
impl Drop for TunRouteGuard {
    fn drop(&mut self) {
        for cidr in self.routes.iter().rev() {
            let args = route_delete_args(&self.interface, cidr);
            if let Err(error) = run_command("route", &args) {
                eprintln!(
                    "warning: failed to remove TUN route {cidr} from {}: {error:#}",
                    self.interface
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) struct TunRouteGuard {
    interface: String,
    routes: Vec<String>,
}

#[cfg(target_os = "linux")]
impl TunRouteGuard {
    pub(crate) fn install_routes(interface: &str, route_cidrs: &[String]) -> Result<Self> {
        let mut guard = Self {
            interface: interface.to_string(),
            routes: Vec::new(),
        };
        for cidr in route_cidrs {
            let args = linux_route_add_args(interface, cidr);
            run_command("ip", &args)
                .with_context(|| format!("failed to add TUN route {cidr} via {interface}"))?;
            guard.routes.push(cidr.clone());
        }
        Ok(guard)
    }
}

#[cfg(target_os = "linux")]
impl Drop for TunRouteGuard {
    fn drop(&mut self) {
        for cidr in self.routes.iter().rev() {
            let args = linux_route_delete_args(&self.interface, cidr);
            if let Err(error) = run_command("ip", &args) {
                eprintln!(
                    "warning: failed to remove TUN route {cidr} from {}: {error:#}",
                    self.interface
                );
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) struct TunRouteGuard {
    interface: String,
    routes: Vec<WindowsIpv4Route>,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct WindowsIpv4Route {
    destination: Ipv4Addr,
    prefix_len: u8,
}

#[cfg(target_os = "windows")]
impl TunRouteGuard {
    pub(crate) fn install_routes(interface: &str, route_cidrs: &[String]) -> Result<Self> {
        let mut guard = Self {
            interface: interface.to_string(),
            routes: Vec::new(),
        };
        for cidr in route_cidrs {
            let (destination, prefix_len) = parse_ipv4_cidr(cidr).with_context(|| {
                format!("invalid TUN route CIDR pushed by Private Access: {cidr}")
            })?;
            add_windows_ipv4_route(interface, destination, prefix_len)
                .with_context(|| format!("failed to add TUN route {cidr} via {interface}"))?;
            guard.routes.push(WindowsIpv4Route {
                destination,
                prefix_len,
            });
        }
        Ok(guard)
    }
}

#[cfg(target_os = "windows")]
impl Drop for TunRouteGuard {
    fn drop(&mut self) {
        for route in self.routes.iter().rev() {
            if let Err(error) =
                delete_windows_ipv4_route(&self.interface, route.destination, route.prefix_len)
            {
                eprintln!(
                    "warning: failed to remove TUN route {}/{} from {}: {error:#}",
                    route.destination, route.prefix_len, self.interface
                );
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn add_windows_ipv4_route(interface: &str, destination: Ipv4Addr, prefix_len: u8) -> Result<()> {
    let row = windows_ipv4_route_row(interface, destination, prefix_len)?;
    win32_route_result(unsafe {
        windows::Win32::NetworkManagement::IpHelper::CreateIpForwardEntry2(&row)
    })
    .context("CreateIpForwardEntry2 failed")
}

#[cfg(target_os = "windows")]
fn delete_windows_ipv4_route(interface: &str, destination: Ipv4Addr, prefix_len: u8) -> Result<()> {
    let row = windows_ipv4_route_row(interface, destination, prefix_len)?;
    win32_route_result(unsafe {
        windows::Win32::NetworkManagement::IpHelper::DeleteIpForwardEntry2(&row)
    })
    .context("DeleteIpForwardEntry2 failed")
}

#[cfg(target_os = "windows")]
fn windows_ipv4_route_row(
    interface: &str,
    destination: Ipv4Addr,
    prefix_len: u8,
) -> Result<windows::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_ROW2> {
    use windows::Win32::NetworkManagement::IpHelper::{
        ConvertInterfaceAliasToLuid, IP_ADDRESS_PREFIX, InitializeIpForwardEntry,
        MIB_IPFORWARD_ROW2,
    };
    use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
    use windows::Win32::Networking::WinSock::{MIB_IPPROTO_NETMGMT, NlroManual};
    use windows::core::PCWSTR;

    let mut luid = NET_LUID_LH::default();
    let interface_wide: Vec<u16> = interface.encode_utf16().chain(std::iter::once(0)).collect();
    win32_route_result(unsafe {
        ConvertInterfaceAliasToLuid(PCWSTR(interface_wide.as_ptr()), &mut luid)
    })
    .with_context(|| format!("failed to resolve Windows interface alias {interface}"))?;

    let mut row = MIB_IPFORWARD_ROW2::default();
    unsafe {
        InitializeIpForwardEntry(&mut row);
    }
    row.InterfaceLuid = luid;
    row.InterfaceIndex = 0;
    row.DestinationPrefix = IP_ADDRESS_PREFIX {
        Prefix: windows_sockaddr_inet_v4(destination),
        PrefixLength: prefix_len,
    };
    // 0.0.0.0 creates an on-link route bound directly to the TUN interface.
    row.NextHop = windows_sockaddr_inet_v4(Ipv4Addr::UNSPECIFIED);
    row.Metric = 1;
    row.Protocol = MIB_IPPROTO_NETMGMT;
    row.Origin = NlroManual;
    Ok(row)
}

#[cfg(target_os = "windows")]
fn win32_route_result(error: windows::Win32::Foundation::WIN32_ERROR) -> Result<()> {
    if error.0 == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(error.0 as i32))
            .context(format!("Windows route API returned error {}", error.0))
    }
}

#[cfg(target_os = "windows")]
fn windows_sockaddr_inet_v4(addr: Ipv4Addr) -> windows::Win32::Networking::WinSock::SOCKADDR_INET {
    use windows::Win32::Networking::WinSock::{
        AF_INET, IN_ADDR, IN_ADDR_0, IN_ADDR_0_0, SOCKADDR_IN, SOCKADDR_INET,
    };

    let [s_b1, s_b2, s_b3, s_b4] = addr.octets();
    SOCKADDR_INET {
        Ipv4: SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: 0,
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_un_b: IN_ADDR_0_0 {
                        s_b1,
                        s_b2,
                        s_b3,
                        s_b4,
                    },
                },
            },
            sin_zero: [0; 8],
        },
    }
}

fn parse_ipv4_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (address, prefix_len) = cidr
        .split_once('/')
        .with_context(|| format!("CIDR {cidr} is missing '/'"))?;
    let address: Ipv4Addr = address
        .parse()
        .with_context(|| format!("CIDR {cidr} has invalid IPv4 address"))?;
    let prefix_len: u8 = prefix_len
        .parse()
        .with_context(|| format!("CIDR {cidr} has invalid prefix length"))?;
    if prefix_len > 32 {
        bail!("CIDR {cidr} has IPv4 prefix length greater than 32");
    }
    Ok((ipv4_network_address(address, prefix_len), prefix_len))
}

fn ipv4_network_address(address: Ipv4Addr, prefix_len: u8) -> Ipv4Addr {
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    };
    Ipv4Addr::from(u32::from(address) & mask)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) struct TunRouteGuard;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
impl TunRouteGuard {
    pub(crate) fn install_routes(_interface: &str, _route_cidrs: &[String]) -> Result<Self> {
        bail!(
            "Private Access pushed route installation currently supports Linux, macOS, and Windows only"
        )
    }
}

#[cfg(target_os = "linux")]
fn linux_route_add_args(interface: &str, cidr: &str) -> Vec<String> {
    vec![
        "route".to_string(),
        "add".to_string(),
        cidr.to_string(),
        "dev".to_string(),
        interface.to_string(),
    ]
}

#[cfg(target_os = "linux")]
fn linux_route_delete_args(interface: &str, cidr: &str) -> Vec<String> {
    vec![
        "route".to_string(),
        "del".to_string(),
        cidr.to_string(),
        "dev".to_string(),
        interface.to_string(),
    ]
}

#[cfg(target_os = "macos")]
pub(super) fn route_add_args(interface: &str, cidr: &str) -> Vec<String> {
    vec![
        "-n".to_string(),
        "add".to_string(),
        "-net".to_string(),
        cidr.to_string(),
        "-interface".to_string(),
        interface.to_string(),
    ]
}

#[cfg(target_os = "macos")]
fn route_delete_args(interface: &str, cidr: &str) -> Vec<String> {
    vec![
        "-n".to_string(),
        "delete".to_string(),
        "-net".to_string(),
        cidr.to_string(),
        "-interface".to_string(),
        interface.to_string(),
    ]
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_command(program: &str, args: &[String]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program} {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{program} {} exited with status {}: {}",
            args.join(" "),
            output.status,
            stderr.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(any(
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos")),
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
    ))]
    use super::write_ipv4_with_retry;
    use super::{
        TunHelperCommand, TunHelperEvent, TunHelperStartConfig, helper_log_suffix,
        is_noninteractive_sudo_command, normalize_dns_namespace, parse_ipv4_cidr,
        remember_helper_log, should_preflight_tun_helper_command, tun_dns_policy_plan,
        tun_route_cidrs_with_dns_servers,
    };

    #[cfg(target_os = "macos")]
    use super::route_add_args;
    #[cfg(target_os = "linux")]
    use super::{linux_resolvectl_configure_args, linux_route_add_args, linux_route_delete_args};
    #[cfg(target_os = "macos")]
    use super::{macos_default_dns_servers_from_scutil, macos_resolver_file_contents};
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn tun_helper_start_command_serializes_as_json_line_protocol() {
        let command = TunHelperCommand::Start {
            config: TunHelperStartConfig {
                client_ipv4: Ipv4Addr::new(10, 250, 252, 93),
                gateway_ipv4: Some(Ipv4Addr::new(10, 250, 252, 1)),
                prefix_len: 22,
                route_cidrs: vec!["10.1.0.0/16".to_string()],
                dns_servers: vec![IpAddr::V4(Ipv4Addr::new(10, 1, 0, 53))],
                dns_namespaces: vec![
                    "service.hundsun.com".to_string(),
                    ".hundsun.com".to_string(),
                ],
                dns_system_namespaces: vec!["sslvpn.hundsun.com".to_string()],
                mtu: Some(1428),
            },
        };

        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&command).expect("serializes"))
                .expect("parses");

        assert_eq!(value["type"], "start");
        assert_eq!(value["config"]["client_ipv4"], "10.250.252.93");
        assert_eq!(value["config"]["gateway_ipv4"], "10.250.252.1");
        assert_eq!(value["config"]["route_cidrs"][0], "10.1.0.0/16");
        assert_eq!(value["config"]["dns_servers"][0], "10.1.0.53");
        assert_eq!(
            value["config"]["dns_namespaces"],
            serde_json::json!(["service.hundsun.com", ".hundsun.com"])
        );
        assert_eq!(
            value["config"]["dns_system_namespaces"],
            serde_json::json!(["sslvpn.hundsun.com"])
        );
        assert_eq!(value["config"]["mtu"], 1428);
    }

    #[test]
    fn tun_helper_packet_event_serializes_payload() {
        let event = TunHelperEvent::Packet {
            payload: "RAAAFA==".to_string(),
        };

        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&event).expect("serializes"))
                .expect("parses");

        assert_eq!(value["type"], "packet");
        assert_eq!(value["payload"], "RAAAFA==");
    }

    #[test]
    fn tun_helper_reset_command_serializes_as_json_line_protocol() {
        let value: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&TunHelperCommand::Reset).expect("serializes"),
        )
        .expect("parses");

        assert_eq!(value["type"], "reset");
    }

    #[cfg(any(
        target_os = "windows",
        all(target_os = "linux", not(target_env = "ohos")),
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
    ))]
    #[test]
    fn tun_write_retries_transient_would_block_instead_of_dropping_packet() {
        let mut attempts = 0;

        write_ipv4_with_retry(64, || {
            attempts += 1;
            if attempts <= 2 {
                Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            } else {
                Ok(64)
            }
        })
        .expect("transient TUN backpressure should recover");

        assert_eq!(attempts, 3);
    }

    #[test]
    fn detects_noninteractive_sudo_helper_command() {
        assert!(is_noninteractive_sudo_command(&[
            "sudo".to_string(),
            "-n".to_string(),
            "/opt/sing-box-tui".to_string(),
            "private-access-tun-helper".to_string(),
            "--stdio".to_string(),
        ]));
        assert!(!is_noninteractive_sudo_command(&[
            "/opt/sing-box-tui".to_string(),
            "private-access-tun-helper".to_string(),
            "--stdio".to_string(),
        ]));
    }

    #[test]
    fn only_default_noninteractive_sudo_helper_is_preflighted() {
        let command = [
            "sudo".to_string(),
            "-n".to_string(),
            "/opt/sing-box-tui".to_string(),
            "private-access-tun-helper".to_string(),
            "--stdio".to_string(),
        ];

        assert!(should_preflight_tun_helper_command(false, &command));
        assert!(!should_preflight_tun_helper_command(true, &command));
    }

    #[test]
    fn helper_log_suffix_keeps_recent_helper_output() {
        let mut logs = Vec::new();
        remember_helper_log(&mut logs, "first");
        remember_helper_log(&mut logs, "second");
        remember_helper_log(&mut logs, "third");
        remember_helper_log(&mut logs, "fourth");

        assert_eq!(
            helper_log_suffix(&logs),
            "; helper output: second | third | fourth"
        );
    }

    #[test]
    fn parses_ipv4_cidr_and_normalizes_network_address() {
        let (address, prefix_len) = parse_ipv4_cidr("10.1.2.3/16").expect("parses CIDR");

        assert_eq!(address, Ipv4Addr::new(10, 1, 0, 0));
        assert_eq!(prefix_len, 16);
    }

    #[test]
    fn rejects_invalid_ipv4_cidr_prefix_len() {
        let error = parse_ipv4_cidr("10.1.2.3/33").expect_err("prefix should be rejected");

        assert!(
            format!("{error:#}").contains("prefix length greater than 32"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn split_dns_namespaces_preserve_exact_and_suffix_matching() {
        assert_eq!(
            normalize_dns_namespace("Service.Hundsun.COM."),
            Some("service.hundsun.com".to_string())
        );
        assert_eq!(
            normalize_dns_namespace("*.Hundsun.COM."),
            Some(".hundsun.com".to_string())
        );
        assert_eq!(
            normalize_dns_namespace(".hs.handsome.com.cn"),
            Some(".hs.handsome.com.cn".to_string())
        );
        assert_eq!(normalize_dns_namespace("10.1.0.53"), None);
        assert_eq!(normalize_dns_namespace("bad domain"), None);
    }

    #[test]
    fn split_dns_keeps_dynamic_suffix_and_overrides_public_gateway_exactly() {
        let plan = tun_dns_policy_plan(
            &[
                ".hundsun.com".to_string(),
                "service.hundsun.com".to_string(),
                "sslvpn.hundsun.com".to_string(),
            ],
            &["SSLVPN.Hundsun.COM.".to_string()],
        );

        assert_eq!(
            plan.tunnel_namespaces,
            vec![".hundsun.com", "service.hundsun.com"]
        );
        assert_eq!(plan.system_namespaces, vec!["sslvpn.hundsun.com"]);
    }

    #[test]
    fn split_dns_does_not_add_unneeded_gateway_override() {
        let plan = tun_dns_policy_plan(
            &[".internal.example.com".to_string()],
            &[
                "vpn.example.com".to_string(),
                ".invalid.example.com".to_string(),
            ],
        );

        assert_eq!(plan.tunnel_namespaces, vec![".internal.example.com"]);
        assert!(plan.system_namespaces.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_split_dns_builds_owned_resolver_file() {
        let contents = macos_resolver_file_contents(
            ".hundsun.com",
            &[
                IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40)),
                IpAddr::V4(Ipv4Addr::new(10, 20, 30, 41)),
            ],
            1234,
        )
        .expect("resolver contents build");

        assert_eq!(
            contents,
            "# sing-box-tui private access pid=1234\n\
             nameserver 10.20.30.40\n\
             nameserver 10.20.30.41\n"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_split_dns_uses_primary_non_supplemental_resolver_for_gateway() {
        let scutil = r#"
resolver #1
  domain   : tail.example
  nameserver[0] : 100.100.100.100
  flags    : Supplemental

resolver #2
  nameserver[0] : 192.168.1.1
  nameserver[1] : 1.1.1.1
  flags    : Request A records

DNS configuration (for scoped queries)
"#;

        assert_eq!(
            macos_default_dns_servers_from_scutil(scutil),
            vec![
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
                IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            ]
        );
    }

    #[test]
    fn dns_servers_receive_host_routes_only_when_not_already_covered() {
        let routes = tun_route_cidrs_with_dns_servers(
            &["10.1.2.3/16".to_string()],
            &[
                IpAddr::V4(Ipv4Addr::new(10, 1, 0, 53)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 10, 53)),
            ],
        )
        .expect("routes normalize");

        assert_eq!(routes, vec!["10.1.0.0/16", "192.168.10.53/32"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn builds_macos_route_add_args_for_pushed_cidr() {
        assert_eq!(
            route_add_args("utun9", "10.1.0.0/16"),
            ["-n", "add", "-net", "10.1.0.0/16", "-interface", "utun9"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn builds_linux_ip_route_args_for_pushed_cidr() {
        assert_eq!(
            linux_route_add_args("tun0", "10.1.0.0/16"),
            ["route", "add", "10.1.0.0/16", "dev", "tun0"]
        );
        assert_eq!(
            linux_route_delete_args("tun0", "10.1.0.0/16"),
            ["route", "del", "10.1.0.0/16", "dev", "tun0"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn builds_linux_systemd_resolved_split_dns_configuration() {
        let commands = linux_resolvectl_configure_args(
            "tun0",
            &[
                ".hundsun.com".to_string(),
                "service.hundsun.com".to_string(),
            ],
            &[
                IpAddr::V4(Ipv4Addr::new(10, 22, 1, 6)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 60, 14)),
            ],
        )
        .expect("Linux split DNS commands build");

        assert_eq!(
            commands,
            vec![
                vec!["dns", "tun0", "10.22.1.6", "192.168.60.14"],
                vec!["domain", "tun0", "~hundsun.com", "~service.hundsun.com"],
                vec!["default-route", "tun0", "false"],
            ]
        );
    }
}
