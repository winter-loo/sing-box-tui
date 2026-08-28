//! macOS privileged process broker for the sing-box TUN process.
//!
//! The TUI stays unprivileged and talks to a root LaunchDaemon over a Unix socket.
//! The daemon authenticates clients by peer UID and exposes only lifecycle operations.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_SOCKET_PATH: &str = "/var/run/sing-box-tui-helper.sock";
const PID_PATH: &str = "/var/run/sing-box-tui-helper.pid";
const MANAGED_LOG_PATH: &str = "/var/log/sing-box-tui-managed.log";

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    Restart { config: PathBuf },
    Stop,
    Status,
    ClearSystemProxy { server: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Response {
    pub(crate) ok: bool,
    pub(crate) pid: Option<u32>,
    pub(crate) error: Option<String>,
}

impl Response {
    fn success(pid: Option<u32>) -> Self {
        Self {
            ok: true,
            pid,
            error: None,
        }
    }

    fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            pid: None,
            error: Some(error.into()),
        }
    }
}

pub(crate) fn helper_available() -> bool {
    Path::new(DEFAULT_SOCKET_PATH).exists()
}

pub(crate) fn restart(config: &Path) -> Result<u32> {
    let response = send_request(
        Path::new(DEFAULT_SOCKET_PATH),
        &Request::Restart {
            config: config.to_path_buf(),
        },
    )?;
    response_result(response)?.context("privileged helper did not return a sing-box pid")
}

pub(crate) fn stop() -> Result<()> {
    response_result(send_request(
        Path::new(DEFAULT_SOCKET_PATH),
        &Request::Stop,
    )?)?;
    Ok(())
}

pub(crate) fn status() -> Result<Option<u32>> {
    response_result(send_request(
        Path::new(DEFAULT_SOCKET_PATH),
        &Request::Status,
    )?)
}

pub(crate) fn clear_system_proxy(server: &str) -> Result<()> {
    response_result(send_request(
        Path::new(DEFAULT_SOCKET_PATH),
        &Request::ClearSystemProxy {
            server: server.to_string(),
        },
    )?)?;
    Ok(())
}

fn response_result(response: Response) -> Result<Option<u32>> {
    if response.ok {
        Ok(response.pid)
    } else {
        bail!(
            "macOS privileged helper rejected request: {}",
            response.error.as_deref().unwrap_or("unknown error")
        )
    }
}

fn send_request(socket: &Path, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "failed to connect to macOS privileged helper at {}",
            socket.display()
        )
    })?;
    serde_json::to_writer(&mut stream, request)
        .context("failed to encode privileged helper request")?;
    stream
        .write_all(b"\n")
        .context("failed to send privileged helper request")?;
    stream
        .flush()
        .context("failed to flush privileged helper request")?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("failed to read privileged helper response")?;
    serde_json::from_str(&line).context("failed to decode privileged helper response")
}

pub(crate) fn serve(socket: &Path, allowed_uid: u32, executable: &Path) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("macOS privileged helper must run as root")
    }
    if socket.exists() {
        fs::remove_file(socket).with_context(|| {
            format!("failed to remove stale helper socket {}", socket.display())
        })?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("failed to bind helper socket {}", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    let chown_status =
        unsafe { libc::chown(path_bytes(socket).as_ptr().cast(), allowed_uid, u32::MAX) };
    if chown_status != 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to assign helper socket owner");
    }

    let executable = validate_root_executable(executable)?;
    listener.set_nonblocking(true)?;
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = Arc::clone(&running);
    ctrlc::set_handler(move || signal_running.store(false, Ordering::SeqCst))?;
    let mut managed = ManagedProcess::recover(&executable)?;
    while running.load(Ordering::SeqCst) {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(_) => continue,
        };
        let response = match handle_client(&mut stream, allowed_uid, &executable, &mut managed) {
            Ok(response) => response,
            Err(error) => Response::failure(format!("{error:#}")),
        };
        if serde_json::to_writer(&mut stream, &response).is_ok() {
            let _ = stream.write_all(b"\n");
        }
    }
    managed.stop()?;
    Ok(())
}

fn handle_client(
    stream: &mut UnixStream,
    allowed_uid: u32,
    executable: &Path,
    managed: &mut ManagedProcess,
) -> Result<Response> {
    let peer_uid = peer_uid(stream)?;
    if peer_uid != allowed_uid && peer_uid != 0 {
        bail!("client uid {peer_uid} is not authorized")
    }
    managed.reap_if_exited()?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?)
        .take(64 * 1024 + 1)
        .read_line(&mut line)
        .context("failed to read helper request")?;
    if line.len() > 64 * 1024 {
        bail!("helper request exceeds 64 KiB")
    }
    match serde_json::from_str::<Request>(&line).context("invalid helper request")? {
        Request::Status => Ok(Response::success(managed.pid())),
        Request::Stop => {
            managed.stop()?;
            Ok(Response::success(None))
        }
        Request::ClearSystemProxy { server } => {
            clear_matching_dynamic_proxy_state(&server)?;
            Ok(Response::success(managed.pid()))
        }
        Request::Restart { config } => {
            let config = validate_user_file(&config, allowed_uid, "config")?;
            validate_config_paths(&config)?;
            managed.stop()?;
            let output = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(MANAGED_LOG_PATH)?;
            let error_output = output.try_clone()?;
            let child = Command::new(&executable)
                .arg("run")
                .arg("--config")
                .arg(&config)
                .current_dir(config.parent().context("config path has no parent")?)
                .stdin(Stdio::null())
                .stdout(Stdio::from(output))
                .stderr(Stdio::from(error_output))
                .spawn()
                .with_context(|| format!("failed to start {}", executable.display()))?;
            let pid = child.id();
            if let Err(error) = fs::write(PID_PATH, format!("{pid}\n{}\n", config.display())) {
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("failed to persist managed sing-box pid");
            }
            *managed = ManagedProcess::Child(child);
            Ok(Response::success(Some(pid)))
        }
    }
}

fn clear_matching_dynamic_proxy_state(server: &str) -> Result<()> {
    let (host, port) = parse_loopback_proxy_server(server)?;
    for service_id in network_connection_service_ids()? {
        let key = format!("State:/Network/Service/{service_id}/Proxies");
        let current = run_scutil(&format!("show {key}\n"))?;
        if !dynamic_proxy_matches(&current, &host, &port) {
            continue;
        }
        run_scutil(&format!("remove {key}\n"))?;
        let remaining = run_scutil(&format!("show {key}\n"))?;
        if dynamic_proxy_matches(&remaining, &host, &port) {
            bail!("network extension republished matching dynamic proxy state")
        }
    }
    Ok(())
}

fn parse_loopback_proxy_server(server: &str) -> Result<(String, String)> {
    let (host, port) = server
        .trim()
        .rsplit_once(':')
        .context("system proxy server must be host:port")?;
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if !matches!(host, "127.0.0.1" | "::1" | "localhost") {
        bail!("privileged dynamic proxy cleanup is restricted to loopback servers")
    }
    let parsed_port = port
        .trim()
        .parse::<u16>()
        .context("system proxy port must be a number")?;
    if parsed_port == 0 {
        bail!("system proxy port must be greater than zero")
    }
    Ok((host.to_string(), port.trim().to_string()))
}

fn network_connection_service_ids() -> Result<Vec<String>> {
    let output = Command::new("/usr/sbin/scutil")
        .args(["--nc", "list"])
        .output()
        .context("failed to list macOS network connections")?;
    if !output.status.success() {
        bail!("scutil --nc list failed with {}", output.status)
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            line.split_whitespace()
                .find(|value| valid_service_id(value))
        })
        .map(ToString::to_string)
        .collect())
}

fn valid_service_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn run_scutil(script: &str) -> Result<String> {
    let mut child = Command::new("/usr/sbin/scutil")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start scutil")?;
    child
        .stdin
        .take()
        .context("failed to open scutil stdin")?
        .write_all(script.as_bytes())?;
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() || stdout.contains("Permission denied") || !stderr.is_empty() {
        let message = if stderr.is_empty() { stdout } else { stderr };
        bail!("scutil failed: {message}")
    }
    Ok(stdout)
}

fn dynamic_proxy_matches(text: &str, host: &str, port: &str) -> bool {
    proxy_value(text, "HTTPEnable") == Some("1")
        && proxy_value(text, "HTTPProxy") == Some(host)
        && proxy_value(text, "HTTPPort") == Some(port)
        && proxy_value(text, "HTTPSEnable") == Some("1")
        && proxy_value(text, "HTTPSProxy") == Some(host)
        && proxy_value(text, "HTTPSPort") == Some(port)
        && proxy_value(text, "SOCKSEnable") == Some("1")
        && proxy_value(text, "SOCKSProxy") == Some(host)
        && proxy_value(text, "SOCKSPort") == Some(port)
}

fn proxy_value<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim() == name).then_some(value.trim())
    })
}

fn validate_root_executable(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .context("failed to resolve sing-box executable")?;
    let metadata = path.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("sing-box executable is not an executable file")
    }
    if metadata.uid() != 0 {
        bail!("sing-box executable must be owned by root")
    }
    if metadata.mode() & 0o022 != 0 {
        bail!("sing-box executable must not be group- or world-writable")
    }
    Ok(path)
}

fn validate_user_file(path: &Path, allowed_uid: u32, label: &str) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("failed to resolve {label} file"))?;
    let metadata = path.metadata()?;
    if !metadata.is_file() || metadata.uid() != allowed_uid {
        bail!("{label} file must be a regular file owned by the authorized user")
    }
    if metadata.mode() & 0o022 != 0 {
        bail!("{label} file must not be group- or world-writable")
    }
    Ok(path)
}

fn validate_config_paths(config: &Path) -> Result<()> {
    let root = config
        .parent()
        .context("config path has no parent")?
        .canonicalize()?;
    let value: serde_json::Value = serde_json::from_slice(&fs::read(config)?)
        .context("privileged helper requires strict JSON config")?;
    validate_value_paths(&value, None, &root)
}

fn validate_value_paths(value: &serde_json::Value, key: Option<&str>, root: &Path) -> Result<()> {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == "path" && object_path_is_transport_url(map) {
                    continue;
                }
                validate_value_paths(value, Some(key), root)?;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                validate_value_paths(value, key, root)?;
            }
        }
        serde_json::Value::String(text) => {
            let looks_like_path = key.is_some_and(|key| key.contains("path") || key == "output")
                || text.starts_with('/')
                || text.starts_with('~');
            if looks_like_path {
                validate_config_path_value(text, root)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn object_path_is_transport_url(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    matches!(
        map.get("type").and_then(serde_json::Value::as_str),
        Some("ws" | "http" | "httpupgrade")
    )
}

fn validate_config_path_value(text: &str, root: &Path) -> Result<()> {
    let path = Path::new(text);
    if path.is_absolute()
        || text.starts_with('~')
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!("privileged sing-box config may only reference paths beneath its config directory")
    }
    let candidate = root.join(path);
    let resolved = if candidate.exists() {
        candidate.canonicalize()?
    } else {
        candidate
            .parent()
            .context("config path has no parent")?
            .canonicalize()?
            .join(
                candidate
                    .file_name()
                    .context("config path has no file name")?,
            )
    };
    if !resolved.starts_with(root) {
        bail!("privileged sing-box config path escapes its config directory")
    }
    Ok(())
}

enum ManagedProcess {
    None,
    Child(Child),
    Recovered {
        pid: u32,
        executable: PathBuf,
        config: PathBuf,
    },
}

impl ManagedProcess {
    fn recover(executable: &Path) -> Result<Self> {
        let Ok(contents) = fs::read_to_string(PID_PATH) else {
            return Ok(Self::None);
        };
        let mut lines = contents.lines();
        let Some(pid) = lines.next().and_then(|v| v.parse::<u32>().ok()) else {
            return Ok(Self::None);
        };
        let Some(config) = lines.next() else {
            return Ok(Self::None);
        };
        if process_matches(pid, executable, Path::new(config))? {
            Ok(Self::Recovered {
                pid,
                executable: executable.to_path_buf(),
                config: PathBuf::from(config),
            })
        } else {
            let _ = fs::remove_file(PID_PATH);
            Ok(Self::None)
        }
    }

    fn pid(&self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Child(c) => Some(c.id()),
            Self::Recovered { pid, .. } => Some(*pid),
        }
    }

    fn reap_if_exited(&mut self) -> Result<()> {
        let exited = match self {
            Self::None => false,
            Self::Child(c) => c.try_wait()?.is_some(),
            Self::Recovered {
                pid,
                executable,
                config,
            } => !process_matches(*pid, executable, config)?,
        };
        if exited {
            *self = Self::None;
            let _ = fs::remove_file(PID_PATH);
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        let Some(pid) = self.pid() else { return Ok(()) };
        if let Self::Recovered {
            executable, config, ..
        } = self
        {
            if !process_matches(pid, executable, config)? {
                *self = Self::None;
                let _ = fs::remove_file(PID_PATH);
                return Ok(());
            }
        }
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut terminated = false;
        while Instant::now() < deadline {
            let exited = match self {
                Self::Child(child) => child.try_wait()?.is_some(),
                Self::Recovered {
                    executable, config, ..
                } => !process_matches(pid, executable, config)?,
                Self::None => true,
            };
            if exited {
                terminated = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let safe_to_force = match self {
            Self::Recovered {
                executable, config, ..
            } => process_matches(pid, executable, config)?,
            Self::Child(child) => child.id() == pid,
            Self::None => false,
        };
        if !terminated && safe_to_force && unsafe { libc::kill(pid as i32, 0) } == 0 {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
        if let Self::Child(child) = self {
            if child.try_wait()?.is_none() {
                let _ = child.wait();
            }
        }
        *self = Self::None;
        let _ = fs::remove_file(PID_PATH);
        Ok(())
    }
}

fn process_matches(pid: u32, executable: &Path, config: &Path) -> Result<bool> {
    let output = Command::new("/bin/ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let command = String::from_utf8_lossy(&output.stdout);
    Ok(
        command.starts_with(&executable.to_string_lossy().to_string())
            && command.contains(&config.to_string_lossy().to_string()),
    )
}

fn peer_uid(stream: &UnixStream) -> Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    let result =
        unsafe { libc::getpeereid(std::os::fd::AsRawFd::as_raw_fd(stream), &mut uid, &mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to authenticate helper client");
    }
    Ok(uid)
}

fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_never_accepts_an_arbitrary_command() {
        let request = r#"{"action":"run","command":["sh","-c","id"]}"#;
        assert!(serde_json::from_str::<Request>(request).is_err());
    }

    #[test]
    fn proxy_cleanup_protocol_accepts_only_a_server_not_a_dynamic_store_key() {
        let request = r#"{"action":"clear_system_proxy","server":"127.0.0.1:6780"}"#;
        assert!(matches!(
            serde_json::from_str::<Request>(request),
            Ok(Request::ClearSystemProxy { server }) if server == "127.0.0.1:6780"
        ));
        assert!(parse_loopback_proxy_server("192.168.10.2:6780").is_err());
        assert!(parse_loopback_proxy_server("127.0.0.1:6780").is_ok());
    }

    #[test]
    fn config_path_validation_rejects_root_file_targets() {
        let root = std::env::temp_dir();
        let value = serde_json::json!({"log": {"output": "/etc/sudoers"}});
        assert!(validate_value_paths(&value, None, &root).is_err());
    }

    #[test]
    fn config_path_validation_accepts_transport_url_paths() {
        let root = std::env::temp_dir();
        let value = serde_json::json!({
            "transport": {"type": "ws", "path": "/images"}
        });
        validate_value_paths(&value, None, &root).expect("WebSocket path is not a file path");
    }

    #[test]
    fn executable_validation_rejects_non_root_owned_files() {
        let root =
            std::env::temp_dir().join(format!("sing-box-helper-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let executable = root.join("sing-box");
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o777)).unwrap();
        let error = validate_root_executable(&executable).unwrap_err();
        assert!(error.to_string().contains("owned by root"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stopping_a_cooperative_child_does_not_wait_for_the_force_kill_deadline() {
        let child = Command::new("/bin/sh")
            .args(["-c", "trap 'exit 0' TERM; while :; do sleep 0.05; done"])
            .spawn()
            .expect("cooperative child starts");
        let mut managed = ManagedProcess::Child(child);
        let started = Instant::now();
        managed.stop().expect("cooperative child stops");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cooperative stop took {:?}",
            started.elapsed()
        );
    }
}
