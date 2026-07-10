use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::Ipv4Addr;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

const TUN_HELPER_READY_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TunHelperStartConfig {
    pub(crate) client_ipv4: Ipv4Addr,
    pub(crate) gateway_ipv4: Ipv4Addr,
    pub(crate) prefix_len: u32,
    pub(crate) route_cidrs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TunHelperCommand {
    Start { config: TunHelperStartConfig },
    Packet { payload: String },
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
        client.wait_ready()?;
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

    fn wait_ready(&mut self) -> Result<()> {
        let deadline = std::time::Instant::now() + TUN_HELPER_READY_TIMEOUT;
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
        let device = tun_rs::DeviceBuilder::new()
            .ipv4(
                config.client_ipv4,
                config.prefix_len as u8,
                Some(config.gateway_ipv4),
            )
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
        #[cfg(unix)]
        device
            .set_nonblocking(true)
            .context("failed to set TUN helper device nonblocking")?;
        let interface = device.name().context("failed to read TUN interface name")?;
        let routes = TunRouteGuard::install_routes(&interface, &config.route_cidrs)
            .context("failed to install pushed TUN routes")?;
        let device = Arc::new(device);
        let reader_device = Arc::clone(&device);
        let shutdown = Arc::new(AtomicBool::new(false));
        let reader_shutdown = Arc::clone(&shutdown);
        let reader = thread::spawn(move || {
            let mut buffer = vec![0_u8; 65535];
            while !reader_shutdown.load(Ordering::SeqCst) {
                match reader_device.recv(&mut buffer) {
                    Ok(size) => {
                        let packet = &buffer[..size];
                        if packet.first().map(|byte| byte >> 4) != Some(4) {
                            let _ = emit_tun_helper_event(
                                &stdout,
                                &TunHelperEvent::Log {
                                    message: "dropped non-IPv4 packet from TUN".to_string(),
                                },
                            );
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
                        thread::sleep(Duration::from_millis(10));
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
            _routes: routes,
            shutdown,
            reader: Some(reader),
        })
    }

    fn write_ipv4(&self, packet: &[u8]) -> Result<()> {
        if packet.first().map(|byte| byte >> 4) != Some(4) {
            bail!("TUN helper write requires an inner IPv4 packet");
        }
        match self.device.send(packet) {
            Ok(size) if size == packet.len() => Ok(()),
            Ok(size) => bail!(
                "short TUN helper write: wrote {size} of {} bytes",
                packet.len()
            ),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error).context("failed to write IPv4 packet to TUN"),
        }
    }

    fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
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

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) struct TunRouteGuard;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl TunRouteGuard {
    pub(crate) fn install_routes(_interface: &str, _route_cidrs: &[String]) -> Result<Self> {
        bail!("Private Access pushed route installation currently supports macOS and Windows only")
    }
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

#[cfg(target_os = "macos")]
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
    use super::{
        TunHelperCommand, TunHelperEvent, TunHelperStartConfig, helper_log_suffix,
        is_noninteractive_sudo_command, parse_ipv4_cidr, remember_helper_log,
        should_preflight_tun_helper_command,
    };

    #[cfg(target_os = "macos")]
    use super::route_add_args;
    use std::net::Ipv4Addr;

    #[test]
    fn tun_helper_start_command_serializes_as_json_line_protocol() {
        let command = TunHelperCommand::Start {
            config: TunHelperStartConfig {
                client_ipv4: Ipv4Addr::new(10, 250, 252, 93),
                gateway_ipv4: Ipv4Addr::new(10, 250, 252, 1),
                prefix_len: 22,
                route_cidrs: vec!["10.1.0.0/16".to_string()],
            },
        };

        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&command).expect("serializes"))
                .expect("parses");

        assert_eq!(value["type"], "start");
        assert_eq!(value["config"]["client_ipv4"], "10.250.252.93");
        assert_eq!(value["config"]["gateway_ipv4"], "10.250.252.1");
        assert_eq!(value["config"]["route_cidrs"][0], "10.1.0.0/16");
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

    #[cfg(target_os = "macos")]
    #[test]
    fn builds_macos_route_add_args_for_pushed_cidr() {
        assert_eq!(
            route_add_args("utun9", "10.1.0.0/16"),
            ["-n", "add", "-net", "10.1.0.0/16", "-interface", "utun9"]
        );
    }
}
