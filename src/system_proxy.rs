use std::collections::BTreeSet;
use std::env;
use std::fs;
#[cfg(target_os = "macos")]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::{self, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

const STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
// Keep RFC1918 ranges out of the OS-level bypass list. If the OS bypasses them before traffic
// reaches sing-box, sing-box cannot apply route overrides for Private Access bridges.
const DEFAULT_BYPASS: &[&str] = &["localhost", "127.*"];

pub(crate) struct SystemProxy {
    config_path: PathBuf,
    server: String,
    server_override: bool,
    enabled: bool,
    enabled_intent: Option<bool>,
    update: Option<SystemProxyJob>,
    last_status_refresh: Instant,
    platform: Arc<dyn SystemProxyPlatform>,
}

pub(crate) enum SystemProxyToggle {
    AlreadyRunning,
    Started { enable: bool, server: String },
}

pub(crate) enum SystemProxyUpdate {
    Applied(String),
    Failed(String),
}

struct SystemProxyJob {
    server: String,
    enable: bool,
    receiver: mpsc::Receiver<Result<String, String>>,
    worker: JoinHandle<()>,
}

trait SystemProxyPlatform: Send + Sync {
    fn apply(&self, server: &str, enable: bool, bypass_entries: &[String]) -> Result<String>;
    fn matches(&self, server: &str) -> bool;
}

struct OsSystemProxyPlatform;

impl SystemProxyPlatform for OsSystemProxyPlatform {
    fn apply(&self, server: &str, enable: bool, bypass_entries: &[String]) -> Result<String> {
        run_system_proxy_update(server, enable, bypass_entries)
    }

    fn matches(&self, server: &str) -> bool {
        system_proxy_matches(server)
    }
}

impl SystemProxy {
    pub(crate) fn new(config_path: impl Into<PathBuf>) -> Self {
        Self::with_platform(config_path.into(), Arc::new(OsSystemProxyPlatform))
    }

    fn with_platform(config_path: PathBuf, platform: Arc<dyn SystemProxyPlatform>) -> Self {
        let server = default_system_proxy_server(&config_path);
        let enabled = platform.matches(&server);
        Self {
            config_path,
            server,
            server_override: false,
            enabled,
            enabled_intent: None,
            update: None,
            last_status_refresh: Instant::now() - STATUS_REFRESH_INTERVAL,
            platform,
        }
    }

    pub(crate) fn server(&self) -> &str {
        &self.server
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn is_updating(&self) -> bool {
        self.update.is_some()
    }

    pub(crate) fn restore_enabled_intent(&mut self, enabled: Option<bool>) {
        self.enabled_intent = enabled;
    }

    pub(crate) fn persisted_enabled(&self) -> Option<bool> {
        Some(self.enabled_intent.unwrap_or(self.enabled))
    }

    pub(crate) fn reconcile_persisted(&mut self, bypass_entries: Vec<String>) -> Result<bool> {
        let Some(desired) = self.enabled_intent else {
            return Ok(false);
        };
        self.sync_detected_server();
        self.enabled = self.platform.matches(&self.server);
        if self.enabled == desired {
            return Ok(false);
        }
        self.apply_verified(desired, &effective_bypass_entries(&bypass_entries))?;
        Ok(true)
    }

    pub(crate) fn suspend_for_exit(&mut self) -> Result<bool> {
        if self.update.is_some() {
            bail!("cannot suspend system proxy while an update is running");
        }
        self.sync_detected_server();
        self.enabled = self.platform.matches(&self.server);
        if !self.enabled && self.enabled_intent != Some(true) {
            return Ok(false);
        }
        self.apply_verified(false, &[])?;
        Ok(true)
    }

    pub(crate) fn server_is_overridden(&self) -> bool {
        self.server_override
    }

    pub(crate) fn override_server(&mut self, server: String) {
        self.restore_server(server, true);
    }

    pub(crate) fn restore_server(&mut self, server: String, server_override: bool) {
        self.server = server;
        self.server_override = server_override;
        self.enabled = self.platform.matches(&self.server);
    }

    pub(crate) fn resolved_server(&mut self) -> String {
        self.sync_detected_server();
        self.server.clone()
    }

    pub(crate) fn toggle(&mut self, bypass_entries: Vec<String>) -> SystemProxyToggle {
        if self.update.is_some() {
            return SystemProxyToggle::AlreadyRunning;
        }
        self.sync_detected_server();
        self.enabled = self.platform.matches(&self.server);
        let enable = !self.enabled;
        let server = self.server.clone();
        let worker_server = server.clone();
        let bypass_entries = effective_bypass_entries(&bypass_entries);
        let platform = Arc::clone(&self.platform);
        let (tx, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = platform
                .apply(&worker_server, enable, &bypass_entries)
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        self.update = Some(SystemProxyJob {
            server: server.clone(),
            enable,
            receiver,
            worker,
        });
        SystemProxyToggle::Started { enable, server }
    }

    pub(crate) fn poll(&mut self) -> Option<SystemProxyUpdate> {
        let Some(job) = self.update.as_ref() else {
            self.refresh_status_if_due();
            return None;
        };
        let result = match job.receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("system proxy worker disconnected".to_string()),
        };

        let job = self.update.take().expect("system proxy job exists");
        let _ = job.worker.join();
        self.server = job.server;
        match result {
            Ok(message) => {
                self.enabled = self.platform.matches(&self.server);
                if self.enabled == job.enable {
                    self.enabled_intent = Some(job.enable);
                    Some(SystemProxyUpdate::Applied(message))
                } else {
                    Some(SystemProxyUpdate::Failed(format!(
                        "system proxy command completed but observed state is {}",
                        if self.enabled { "enabled" } else { "disabled" }
                    )))
                }
            }
            Err(error) => {
                self.enabled = self.platform.matches(&self.server);
                Some(SystemProxyUpdate::Failed(error))
            }
        }
    }

    fn refresh_status_if_due(&mut self) {
        if self.update.is_some() || self.last_status_refresh.elapsed() < STATUS_REFRESH_INTERVAL {
            return;
        }
        self.last_status_refresh = Instant::now();
        self.sync_detected_server();
        self.enabled = self.platform.matches(&self.server);
    }

    pub(crate) fn refresh_bypass(
        &mut self,
        base_entries: &[String],
        dynamic_entries: &[String],
    ) -> Result<bool> {
        if !self.enabled || self.update.is_some() {
            return Ok(false);
        }
        let entries =
            effective_bypass_entries(&merge_bypass_sources(base_entries, dynamic_entries));
        self.platform
            .apply(&self.server, true, &entries)
            .context("failed to refresh system proxy with Private Access bypass rules")?;
        self.enabled = self.platform.matches(&self.server);
        Ok(true)
    }

    fn sync_detected_server(&mut self) {
        if !self.server_override {
            self.server = default_system_proxy_server(&self.config_path);
        }
    }

    fn apply_verified(&mut self, enable: bool, bypass_entries: &[String]) -> Result<String> {
        let message = self.platform.apply(&self.server, enable, bypass_entries)?;
        self.enabled = self.platform.matches(&self.server);
        if self.enabled != enable {
            bail!(
                "system proxy command completed but observed state is {}",
                if self.enabled { "enabled" } else { "disabled" }
            );
        }
        Ok(message)
    }

    #[cfg(test)]
    pub(crate) fn for_test(config_path: impl Into<PathBuf>, server: &str, enabled: bool) -> Self {
        let platform = Arc::new(FixedSystemProxyPlatform {
            enabled: std::sync::atomic::AtomicBool::new(enabled),
            fail_updates: false,
        });
        let mut proxy = Self::with_platform(config_path.into(), platform);
        proxy.server = server.to_string();
        proxy.enabled = enabled;
        proxy
    }

    #[cfg(test)]
    pub(crate) fn failing_for_test(
        config_path: impl Into<PathBuf>,
        server: &str,
        enabled: bool,
    ) -> Self {
        let platform = Arc::new(FixedSystemProxyPlatform {
            enabled: std::sync::atomic::AtomicBool::new(enabled),
            fail_updates: true,
        });
        let mut proxy = Self::with_platform(config_path.into(), platform);
        proxy.server = server.to_string();
        proxy.enabled = enabled;
        proxy
    }
}

fn merge_bypass_sources(base_entries: &[String], dynamic_entries: &[String]) -> Vec<String> {
    let mut entries = base_entries.to_vec();
    for entry in dynamic_entries {
        if !entries.contains(entry) {
            entries.push(entry.clone());
        }
    }
    entries
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

fn effective_bypass_entries(entries: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut bypass = Vec::new();
    for entry in DEFAULT_BYPASS {
        push_unique_bypass_value(&mut bypass, &mut seen, entry);
    }
    for octet in 64..=127 {
        push_unique_bypass_value(&mut bypass, &mut seen, &format!("100.{octet}.*"));
    }
    for entry in entries {
        push_bypass_entry(&mut bypass, &mut seen, entry);
    }
    bypass
}

fn push_bypass_entry(bypass: &mut Vec<String>, seen: &mut BTreeSet<String>, entry: &str) {
    let entry = entry.trim();
    if entry.is_empty() {
        return;
    }
    push_unique_bypass_value(bypass, seen, entry);
    if !entry.contains('*')
        && !entry.contains('/')
        && !entry.starts_with('<')
        && entry.parse::<std::net::IpAddr>().is_err()
    {
        push_unique_bypass_value(bypass, seen, &format!("*.{entry}"));
    }
}

fn push_unique_bypass_value(bypass: &mut Vec<String>, seen: &mut BTreeSet<String>, value: &str) {
    let value = value.trim().to_ascii_lowercase();
    if !value.is_empty() && seen.insert(value.clone()) {
        bypass.push(value);
    }
}

#[cfg(windows)]
fn wininet_proxy_override_entries(entries: &[String]) -> Vec<String> {
    let mut entries = entries.to_vec();
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

    let (host, port) = parse_proxy_server(server)?;
    if enable {
        for service in &services {
            run_networksetup(&["-setwebproxy", service, &host, &port])?;
            run_networksetup(&["-setsecurewebproxy", service, &host, &port])?;
            run_networksetup(&["-setsocksfirewallproxy", service, &host, &port])?;
            let mut args = vec!["-setproxybypassdomains", service.as_str()];
            args.extend(bypass_entries.iter().map(String::as_str));
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
        let cleaned_connections = clear_macos_legacy_dynamic_proxies(&host, &port)?;
        let cleanup = if cleaned_connections.is_empty() {
            String::new()
        } else {
            format!(
                "; cleared legacy dynamic proxy for {}",
                cleaned_connections.join(", ")
            )
        };
        Ok(format!(
            "Disabled macOS system proxy for {}{cleanup}",
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
        let ignore_hosts = gsettings_string_list_value(bypass_entries);
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
    let managed_service_matches = macos_system_proxy_services().is_ok_and(|services| {
        all_macos_services_match(&services, |service| {
            [
                "-getwebproxy",
                "-getsecurewebproxy",
                "-getsocksfirewallproxy",
            ]
            .iter()
            .all(|action| macos_service_proxy_matches(action, service, &host, &port))
        })
    });
    managed_service_matches || macos_legacy_dynamic_proxy_matches(&host, &port)
}

#[cfg(target_os = "macos")]
fn all_macos_services_match(
    services: &[String],
    mut service_matches: impl FnMut(&str) -> bool,
) -> bool {
    !services.is_empty() && services.iter().all(|service| service_matches(service))
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

    let services = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("An asterisk"))
        .filter(|line| !line.starts_with('*'))
        .map(ToString::to_string)
        .collect();
    let connections = macos_network_connections()?;
    Ok(exclude_macos_connection_services(services, &connections))
}

#[cfg(target_os = "macos")]
fn exclude_macos_connection_services(
    services: Vec<String>,
    connections: &[MacosNetworkConnection],
) -> Vec<String> {
    let connection_names = connections
        .iter()
        .map(|connection| connection.name.as_str())
        .collect::<BTreeSet<_>>();
    services
        .into_iter()
        .filter(|service| !connection_names.contains(service.as_str()))
        .collect()
}

#[cfg(target_os = "macos")]
#[derive(Debug, Eq, PartialEq)]
struct MacosNetworkConnection {
    id: String,
    name: String,
}

#[cfg(target_os = "macos")]
fn macos_network_connections() -> Result<Vec<MacosNetworkConnection>> {
    let output = Command::new("scutil")
        .args(["--nc", "list"])
        .output()
        .context("failed to list macOS network connection services")?;
    if !output.status.success() {
        bail!("scutil --nc list exited with {}", output.status);
    }
    Ok(macos_network_connections_from_scutil(
        &String::from_utf8_lossy(&output.stdout),
    ))
}

#[cfg(target_os = "macos")]
fn macos_network_connections_from_scutil(text: &str) -> Vec<MacosNetworkConnection> {
    text.lines()
        .filter_map(|line| {
            let id = line
                .split_whitespace()
                .find(|value| valid_macos_service_id(value))?;
            let (before_last_quote, _) = line.rsplit_once('"')?;
            let (_, name) = before_last_quote.rsplit_once('"')?;
            Some(MacosNetworkConnection {
                id: id.to_string(),
                name: name.to_string(),
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn valid_macos_service_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[cfg(target_os = "macos")]
fn macos_legacy_dynamic_proxy_matches(host: &str, port: &str) -> bool {
    macos_network_connections().is_ok_and(|connections| {
        connections.iter().any(|connection| {
            read_macos_dynamic_proxy(&connection.id)
                .is_ok_and(|text| macos_dynamic_proxy_matches(&text, host, port))
        })
    })
}

#[cfg(target_os = "macos")]
fn clear_macos_legacy_dynamic_proxies(host: &str, port: &str) -> Result<Vec<String>> {
    let mut matching = Vec::new();
    for connection in macos_network_connections()? {
        let current = read_macos_dynamic_proxy(&connection.id)?;
        if macos_dynamic_proxy_matches(&current, host, port) {
            matching.push(connection);
        }
    }
    if matching.is_empty() {
        return Ok(Vec::new());
    }

    let server = format!("{host}:{port}");
    if crate::macos_privileged_helper::helper_available() {
        crate::macos_privileged_helper::clear_system_proxy(&server)?;
    } else {
        for connection in &matching {
            let key = macos_dynamic_proxy_key(&connection.id);
            run_scutil_script(&format!("remove {key}\n"))?;
        }
    }

    for connection in &matching {
        let remaining = read_macos_dynamic_proxy(&connection.id)?;
        if macos_dynamic_proxy_matches(&remaining, host, port) {
            bail!(
                "macOS network connection {} retained its legacy dynamic proxy",
                connection.name
            );
        }
    }
    Ok(matching
        .into_iter()
        .map(|connection| connection.name)
        .collect())
}

#[cfg(target_os = "macos")]
fn read_macos_dynamic_proxy(service_id: &str) -> Result<String> {
    let key = macos_dynamic_proxy_key(service_id);
    run_scutil_script(&format!("show {key}\n"))
}

#[cfg(target_os = "macos")]
fn macos_dynamic_proxy_key(service_id: &str) -> String {
    format!("State:/Network/Service/{service_id}/Proxies")
}

#[cfg(target_os = "macos")]
fn run_scutil_script(script: &str) -> Result<String> {
    let mut child = Command::new("scutil")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start scutil")?;
    child
        .stdin
        .take()
        .context("failed to open scutil stdin")?
        .write_all(script.as_bytes())
        .context("failed to write scutil command")?;
    let output = child
        .wait_with_output()
        .context("failed to wait for scutil")?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() || stdout.contains("Permission denied") || !stderr.is_empty() {
        let message = if stderr.is_empty() { stdout } else { stderr };
        bail!("scutil update failed: {message}");
    }
    Ok(stdout)
}

#[cfg(target_os = "macos")]
fn macos_dynamic_proxy_matches(text: &str, host: &str, port: &str) -> bool {
    macos_proxy_value(text, "HTTPEnable") == Some("1")
        && macos_proxy_value(text, "HTTPProxy") == Some(host)
        && macos_proxy_value(text, "HTTPPort") == Some(port)
        && macos_proxy_value(text, "HTTPSEnable") == Some("1")
        && macos_proxy_value(text, "HTTPSProxy") == Some(host)
        && macos_proxy_value(text, "HTTPSPort") == Some(port)
        && macos_proxy_value(text, "SOCKSEnable") == Some("1")
        && macos_proxy_value(text, "SOCKSProxy") == Some(host)
        && macos_proxy_value(text, "SOCKSPort") == Some(port)
}

#[cfg(target_os = "macos")]
fn macos_service_proxy_matches(action: &str, service: &str, host: &str, port: &str) -> bool {
    let Ok(output) = Command::new("networksetup")
        .args([action, service])
        .output()
    else {
        return false;
    };
    output.status.success()
        && macos_network_proxy_matches(&String::from_utf8_lossy(&output.stdout), host, port)
}

#[cfg(target_os = "macos")]
fn macos_network_proxy_matches(text: &str, host: &str, port: &str) -> bool {
    macos_proxy_value(text, "Enabled") == Some("Yes")
        && macos_proxy_value(text, "Server") == Some(host)
        && macos_proxy_value(text, "Port") == Some(port)
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

#[cfg(test)]
struct FixedSystemProxyPlatform {
    enabled: std::sync::atomic::AtomicBool,
    fail_updates: bool,
}

#[cfg(test)]
impl SystemProxyPlatform for FixedSystemProxyPlatform {
    fn apply(&self, server: &str, enable: bool, _bypass_entries: &[String]) -> Result<String> {
        if self.fail_updates {
            bail!("injected system proxy update failure");
        }
        self.enabled
            .store(enable, std::sync::atomic::Ordering::Relaxed);
        Ok(format!(
            "{} {server}",
            if enable { "enabled" } else { "disabled" }
        ))
    }

    fn matches(&self, _server: &str) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[derive(Default)]
    struct RecordingPlatform {
        enabled: AtomicBool,
        updates: Mutex<Vec<(String, bool, Vec<String>)>>,
    }

    impl SystemProxyPlatform for RecordingPlatform {
        fn apply(&self, server: &str, enable: bool, bypass_entries: &[String]) -> Result<String> {
            self.updates.lock().expect("updates lock").push((
                server.to_string(),
                enable,
                bypass_entries.to_vec(),
            ));
            self.enabled.store(enable, Ordering::Relaxed);
            Ok(format!("proxy {enable}"))
        }

        fn matches(&self, _server: &str) -> bool {
            self.enabled.load(Ordering::Relaxed)
        }
    }

    fn temp_config(contents: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!("sing-box-tui-system-proxy-{nanos}.json"));
        fs::write(&path, contents).expect("write system proxy config");
        path
    }

    #[test]
    fn interface_detects_mixed_inbound_from_config() {
        let path =
            temp_config(r#"{"inbounds":[{"type":"mixed","listen":"::","listen_port":6780}]}"#);
        let platform = Arc::new(RecordingPlatform::default());

        let proxy = SystemProxy::with_platform(path.clone(), platform);

        assert_eq!(proxy.server(), "127.0.0.1:6780");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn interface_falls_back_to_partial_text_when_config_is_invalid() {
        let path = temp_config(
            r#"{
                "inbounds": [
                    {"type":"mixed","listen":"::","listen_port":6780}
                ],
                "outbounds": [broken
            }"#,
        );
        let platform = Arc::new(RecordingPlatform::default());

        let proxy = SystemProxy::with_platform(path.clone(), platform);

        assert_eq!(proxy.server(), "127.0.0.1:6780");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn idle_poll_refreshes_platform_status() {
        let platform = Arc::new(RecordingPlatform::default());
        let mut proxy = SystemProxy::with_platform(PathBuf::from("missing.json"), platform.clone());
        platform.enabled.store(true, Ordering::Relaxed);

        assert!(proxy.poll().is_none());

        assert!(proxy.enabled());
    }

    #[test]
    fn exit_suspend_preserves_enabled_intent_for_next_reconcile() {
        let platform = Arc::new(RecordingPlatform {
            enabled: AtomicBool::new(true),
            ..RecordingPlatform::default()
        });
        let mut proxy = SystemProxy::with_platform(PathBuf::from("missing.json"), platform.clone());
        proxy.restore_enabled_intent(Some(true));

        proxy.suspend_for_exit().expect("proxy suspends");

        assert!(!proxy.enabled());
        assert_eq!(proxy.persisted_enabled(), Some(true));
        proxy
            .reconcile_persisted(Vec::new())
            .expect("proxy intent restores");
        assert!(proxy.enabled());
        assert_eq!(
            platform
                .updates
                .lock()
                .expect("updates lock")
                .iter()
                .map(|(_, enabled, _)| *enabled)
                .collect::<Vec<_>>(),
            vec![false, true]
        );
    }

    #[test]
    fn exit_suspend_cleans_partially_applied_enabled_intent() {
        let platform = Arc::new(RecordingPlatform::default());
        let mut proxy = SystemProxy::with_platform(PathBuf::from("missing.json"), platform.clone());
        proxy.restore_enabled_intent(Some(true));

        proxy
            .suspend_for_exit()
            .expect("partial proxy state is cleaned");

        assert_eq!(
            platform.updates.lock().expect("updates lock").as_slice(),
            &[("127.0.0.1:6780".to_string(), false, Vec::new())]
        );
        assert_eq!(proxy.persisted_enabled(), Some(true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_service_filter_excludes_vpn_connections() {
        let services = vec![
            "Wi-Fi".to_string(),
            "USB 10/100 LAN".to_string(),
            "Tailscale".to_string(),
        ];
        let connections = r#"
* (Disconnected)   38D78F8E-9EF9-46BD-926E-BEDF0AEC448E PPP --> Modem "Modem" [PPP:Modem]
* (Connected)      E52F0AD5-6C83-41D8-A3FB-0FDE7EE5383C VPN (io.tailscale.ipn.macsys) "Tailscale" [VPN:io.tailscale.ipn.macsys]
"#;

        let connections = macos_network_connections_from_scutil(connections);
        assert_eq!(
            exclude_macos_connection_services(services, &connections),
            vec!["Wi-Fi".to_string(), "USB 10/100 LAN".to_string()]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_status_requires_every_managed_service_to_match() {
        let services = vec!["Wi-Fi".to_string(), "Ethernet".to_string()];

        assert!(!all_macos_services_match(&services, |service| service == "Wi-Fi"));
        assert!(all_macos_services_match(&services, |_| true));
        assert!(!all_macos_services_match(&[], |_| true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_networksetup_state_is_the_status_source() {
        let enabled = r#"
Enabled: Yes
Server: 127.0.0.1
Port: 6780
Authenticated Proxy Enabled: 0
"#;
        let disabled = r#"
Enabled: No
Server: 127.0.0.1
Port: 6780
Authenticated Proxy Enabled: 0
"#;

        assert!(macos_network_proxy_matches(enabled, "127.0.0.1", "6780"));
        assert!(!macos_network_proxy_matches(disabled, "127.0.0.1", "6780"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_connection_parser_keeps_only_valid_dynamic_store_ids() {
        let connections = r#"
Available network connection services in the current set (*=enabled):
* (Connected) E52F0AD5-6C83-41D8-A3FB-0FDE7EE5383C VPN "Tailscale" [VPN:io.tailscale]
* (Disconnected) not/a/key VPN "Unsafe" [VPN:example]
"#;

        assert_eq!(
            macos_network_connections_from_scutil(connections),
            vec![MacosNetworkConnection {
                id: "E52F0AD5-6C83-41D8-A3FB-0FDE7EE5383C".to_string(),
                name: "Tailscale".to_string(),
            }]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_legacy_dynamic_proxy_requires_a_complete_server_match() {
        let stale = r#"
HTTPEnable : 1
HTTPProxy : 127.0.0.1
HTTPPort : 6780
HTTPSEnable : 1
HTTPSProxy : 127.0.0.1
HTTPSPort : 6780
SOCKSEnable : 1
SOCKSProxy : 127.0.0.1
SOCKSPort : 6780
SupplementalMatchDomains : <array> {
  0 :
}
"#;

        assert!(macos_dynamic_proxy_matches(stale, "127.0.0.1", "6780"));
        assert!(!macos_dynamic_proxy_matches(stale, "127.0.0.1", "5780"));
    }

    #[test]
    fn toggle_hides_platform_bypass_formatting() {
        let platform = Arc::new(RecordingPlatform::default());
        let mut proxy = SystemProxy::with_platform(PathBuf::from("missing.json"), platform.clone());

        let start = proxy.toggle(vec![
            "example.com".to_string(),
            "*.github.com".to_string(),
            "10.0.0.0/8".to_string(),
            "1.1.1.1".to_string(),
        ]);
        assert!(matches!(
            start,
            SystemProxyToggle::Started { enable: true, .. }
        ));
        while proxy.poll().is_none() {
            thread::yield_now();
        }

        let updates = platform.updates.lock().expect("updates lock");
        let bypass = &updates[0].2;
        assert!(bypass.contains(&"localhost".to_string()));
        assert!(bypass.contains(&"127.*".to_string()));
        assert!(bypass.contains(&"example.com".to_string()));
        assert!(bypass.contains(&"*.example.com".to_string()));
        assert!(bypass.contains(&"*.github.com".to_string()));
        assert!(bypass.contains(&"10.0.0.0/8".to_string()));
        assert!(bypass.contains(&"1.1.1.1".to_string()));
        assert!(bypass.contains(&"100.64.*".to_string()));
        assert!(bypass.contains(&"100.127.*".to_string()));
        assert!(!bypass.contains(&"100.128.*".to_string()));
        assert!(!bypass.contains(&"10.*".to_string()));
        assert!(!bypass.contains(&"192.168.*".to_string()));
    }

    #[test]
    fn refresh_bypass_merges_private_access_entries_without_persisting_them() {
        let platform = Arc::new(RecordingPlatform {
            enabled: AtomicBool::new(true),
            ..RecordingPlatform::default()
        });
        let mut proxy = SystemProxy::with_platform(PathBuf::from("missing.json"), platform.clone());
        let base = vec!["deeloo.cn".to_string()];
        let dynamic = vec!["hundsun.com".to_string(), "deeloo.cn".to_string()];

        assert!(
            proxy
                .refresh_bypass(&base, &dynamic)
                .expect("refresh bypass")
        );

        let updates = platform.updates.lock().expect("updates lock");
        let bypass = &updates[0].2;
        assert!(bypass.contains(&"deeloo.cn".to_string()));
        assert!(bypass.contains(&"hundsun.com".to_string()));
        assert_eq!(base, vec!["deeloo.cn".to_string()]);
    }
}
