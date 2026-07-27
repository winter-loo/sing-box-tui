use std::env;
use std::path::Path;
#[cfg(any(windows, target_os = "macos"))]
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{ACCEPT, CONNECTION, LOCATION};
use reqwest::redirect::Policy;
use reqwest::{RequestBuilder, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::private_access::{PrivateAccessAuthField, PrivateAccessAuthOption, PrivateAccessSecret};

// The wire codec is staged independently from the live transport. Keeping it isolated lets us
// test every byte boundary before any tunnel connection is attempted.
#[allow(dead_code)]
pub(crate) mod evpn;

const SONICWALL_USER_AGENT: &str = "ConnectTunnel/12.5.0.212 (SonicWall; Windows)";
const SONICWALL_ACCEPT: &str = "*/*";
const SONICWALL_HTTP_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct SonicwallAuthClient {
    client: reqwest::Client,
    gateway: Url,
}

pub(crate) struct SonicwallAuthSession {
    client: reqwest::Client,
    gateway: Url,
    location: Url,
    logon_endpoint: &'static str,
    official_logon_status: Option<u16>,
    logon_capability: SonicwallLogonCapability,
    logon_id: Mutex<LogonIdState>,
}

struct LogonIdState {
    token: Option<PrivateAccessSecret>,
    refresh_count: u64,
    observation_count: u64,
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub(crate) struct SonicwallTeamToken([u8; 16]);

impl SonicwallTeamToken {
    pub(crate) fn expose(&self) -> &[u8; 16] {
        &self.0
    }
}

pub(crate) struct SonicwallEvpnIdentity {
    pub(crate) server: String,
    pub(crate) port: u16,
    pub(crate) team_token: SonicwallTeamToken,
    pub(crate) logon_id_refresh_count: u64,
    pub(crate) logon_id_observation_count: u64,
}

pub(crate) struct SonicwallAgentActivation {
    pub(crate) endpoint: Option<&'static str>,
}

pub(crate) struct SonicwallLicenseProbe {
    pub(crate) endpoint: Option<&'static str>,
    pub(crate) licensed: Option<bool>,
    pub(crate) destroy_connections: Option<bool>,
    pub(crate) status: Option<String>,
}

pub(crate) struct SonicwallConnectionStateProbe {
    pub(crate) endpoint: Option<&'static str>,
    pub(crate) alpns_supported: Option<bool>,
    pub(crate) tunnel_protocol_negotiation: Option<bool>,
    pub(crate) zone_type: Option<String>,
}

pub(crate) struct SonicwallSystemInterrogationProbe {
    pub(crate) endpoint: Option<&'static str>,
    pub(crate) zone_count: Option<usize>,
    pub(crate) zone_keys: Vec<String>,
    pub(crate) unsupported_zone_keys: Vec<String>,
    pub(crate) posted_minimal_response: bool,
    pub(crate) is_ct_allow: Option<bool>,
    pub(crate) zone_command: Option<String>,
    pub(crate) zone_type: Option<String>,
}

pub(crate) struct SonicwallAuthChallenge {
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) fields: Vec<PrivateAccessAuthField>,
    pub(crate) buttons: Vec<String>,
}

pub(crate) enum SonicwallAuthStep {
    Challenge(SonicwallAuthChallenge),
    Authenticated,
    Continue,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SonicwallLogonCapability {
    #[default]
    Unknown,
    Official,
    LegacyAdd,
}

impl SonicwallLogonCapability {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Official => "official",
            Self::LegacyAdd => "legacy_add",
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct SonicwallRealm {
    pub(crate) name: String,
    pub(crate) method: Option<i64>,
}

impl SonicwallAuthClient {
    pub(crate) fn new(
        gateway: &str,
        verify_server_cert: bool,
        http_connect_proxy: Option<&str>,
    ) -> Result<Self> {
        let gateway = normalize_gateway_url(gateway)?;
        let mut builder = reqwest::Client::builder()
            .cookie_store(true)
            .danger_accept_invalid_certs(!verify_server_cert)
            .redirect(Policy::none())
            .http1_only()
            .pool_max_idle_per_host(1)
            .pool_idle_timeout(SONICWALL_HTTP_POOL_IDLE_TIMEOUT)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .timeout(Duration::from_secs(30))
            .user_agent(SONICWALL_USER_AGENT);
        if let Some(proxy) = http_connect_proxy.filter(|proxy| !proxy.trim().is_empty()) {
            let proxy = proxy.trim();
            let proxy = if proxy.contains("://") {
                proxy.to_string()
            } else {
                format!("http://{proxy}")
            };
            builder = builder.proxy(
                reqwest::Proxy::all(&proxy)
                    .context("invalid SonicWall HTTP CONNECT proxy configuration")?,
            );
        } else {
            // A direct-first SonicWall attempt must not silently inherit HTTP_PROXY,
            // HTTPS_PROXY, ALL_PROXY, or another process-level proxy setting.
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .context("failed to build SonicWall HTTPS client")?;
        Ok(Self { client, gateway })
    }

    pub(crate) async fn discover_realms(&self) -> Result<Vec<SonicwallRealm>> {
        let response = self.get_public_json("__api__/config/realms").await;
        let value = match response {
            Ok(value) => value,
            Err(_) => self.get_public_json("__api__/config").await?,
        };
        Ok(parse_realms(&value))
    }

    async fn get_public_json(&self, path: &str) -> Result<Value> {
        let url = self.endpoint(path)?;
        let response = self
            .client
            .get(url)
            .header(ACCEPT, SONICWALL_ACCEPT)
            .send()
            .await?;
        let value = decode_json_response(response, "fetch public configuration").await?;
        Ok(value)
    }

    pub(crate) async fn start_logon(
        &self,
        realm: &str,
        preferred_capability: SonicwallLogonCapability,
    ) -> Result<SonicwallAuthSession> {
        if realm.trim().is_empty() {
            bail!("SonicWall LoginGroup cannot be empty");
        }
        const OFFICIAL_LOGON_ENDPOINT: &str = "__api__/logon";
        const LEGACY_LOGON_ENDPOINT: &str = "__api__/logon/Add";

        let (response, logon_endpoint, official_logon_status, logon_capability) =
            if preferred_capability == SonicwallLogonCapability::LegacyAdd {
                let response = interactive_auth_request(
                    self.client
                        .post(self.endpoint(LEGACY_LOGON_ENDPOINT)?)
                        .header(ACCEPT, SONICWALL_ACCEPT)
                        .json(&json!({ "name": realm })),
                )
                .send()
                .await
                .context("failed to start cached SonicWall legacy logon session")?;
                (
                    response,
                    LEGACY_LOGON_ENDPOINT,
                    None,
                    SonicwallLogonCapability::LegacyAdd,
                )
            } else {
                let official_response = interactive_auth_request(
                    self.client
                        .post(self.endpoint(OFFICIAL_LOGON_ENDPOINT)?)
                        .header(ACCEPT, SONICWALL_ACCEPT)
                        .json(&realm),
                )
                .send()
                .await
                .context("failed to start SonicWall logon session")?;
                let official_logon_status = official_response.status().as_u16();
                if official_response.status().is_success() {
                    (
                        official_response,
                        OFFICIAL_LOGON_ENDPOINT,
                        Some(official_logon_status),
                        SonicwallLogonCapability::Official,
                    )
                } else {
                    // Drain the failed response so reqwest can return the CONNECT/TLS
                    // connection to the pool before the legacy request is sent.
                    let _ = official_response.bytes().await;
                    let fallback_response = interactive_auth_request(
                        self.client
                            .post(self.endpoint(LEGACY_LOGON_ENDPOINT)?)
                            .header(ACCEPT, SONICWALL_ACCEPT)
                            .json(&json!({ "name": realm })),
                    )
                    .send()
                    .await
                    .context("failed to start SonicWall logon session")?;
                    (
                        fallback_response,
                        LEGACY_LOGON_ENDPOINT,
                        Some(official_logon_status),
                        SonicwallLogonCapability::LegacyAdd,
                    )
                }
            };
        let header_location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let value = decode_json_response(response, "start logon session").await?;
        let location = find_string_key(&value, &["location", "Location"])
            .map(ToOwned::to_owned)
            .or(header_location)
            .context("SonicWall logon response did not include a session location")?;
        let location = resolve_same_origin_location(&self.gateway, &location)?;
        let logon_id = find_latest_logon_id(&value).map(PrivateAccessSecret::new);
        let observation_count = u64::from(logon_id.is_some());
        Ok(SonicwallAuthSession {
            client: self.client.clone(),
            gateway: self.gateway.clone(),
            location,
            logon_endpoint,
            official_logon_status,
            logon_capability,
            logon_id: Mutex::new(LogonIdState {
                token: logon_id,
                refresh_count: 0,
                observation_count,
            }),
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.gateway
            .join(path)
            .with_context(|| format!("failed to resolve SonicWall endpoint {path}"))
    }
}

pub(crate) fn default_agent_info() -> Value {
    json!({
        "android": false,
        "chromeos": false,
        "ios": false,
        "linux": false,
        "mac": false,
        "osxlion": false,
        "win": true,
        "arm64": cfg!(target_arch = "aarch64"),
        "captchaCapable": false,
        "dynamicExclusions": true,
        "mobileConnect": false,
        "modernTunnelClient": true,
        "nonInteractiveClient": false,
        "pda": false,
        "x64": cfg!(target_pointer_width = "64"),
        "platform": "Windows",
        "qrcodeCapable": true,
        "userAgent": "SonicWall Connect Tunnel",
        "userLocale": "zh"
    })
}

fn count_zone_interrogation_items(value: &Value) -> usize {
    match value {
        Value::Object(object) => object
            .iter()
            .map(|(key, child)| {
                if is_interrogation_list_key(key) {
                    child.as_array().map_or(0, Vec::len)
                } else {
                    count_zone_interrogation_items(child)
                }
            })
            .sum(),
        Value::Array(items) => items.iter().map(count_zone_interrogation_items).sum(),
        _ => 0,
    }
}

fn collect_zone_interrogation_keys(value: &Value) -> Vec<String> {
    fn visit(value: &Value, keys: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if is_interrogation_list_key(key) {
                        if let Some(items) = child.as_array() {
                            for item in items {
                                if let Some(key) = item
                                    .as_object()
                                    .and_then(|object| object_string(object, &["key", "Key"]))
                                    .map(|key| sanitize_probe_string(&key))
                                    .filter(|key| !key.is_empty())
                                {
                                    if !keys.iter().any(|existing| existing == &key) {
                                        keys.push(key);
                                    }
                                }
                            }
                        }
                    } else {
                        visit(child, keys);
                    }
                }
            }
            Value::Array(items) => {
                for child in items {
                    visit(child, keys);
                }
            }
            _ => {}
        }
    }

    let mut keys = Vec::new();
    visit(value, &mut keys);
    keys.truncate(12);
    keys
}

fn is_interrogation_list_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("zone_interrogation_list")
        || key.eq_ignore_ascii_case("micro_interrogation_list")
}

fn minimal_system_interrogation_response() -> Value {
    let user_home = env::var("USERPROFILE")
        .or_else(|_| env::var("HOME"))
        .unwrap_or_default();
    let system_directory = env::var("SystemRoot")
        .map(|root| format!(r"{root}\System32"))
        .unwrap_or_default();
    let equipment_id = equipment_id();
    json!({
        "type": "EPC",
        "client_info": {
            "client_type": "CT",
            "equipmentID": equipment_id,
            "DEVICE_MACAddress": "",
            "DEVICE_SerialNumber": "",
            "DEVICE_UDID": "",
            "userHomeDirectory": user_home,
            "systemDirectory": system_directory,
            "user": "false"
        },
        "interrogation_info": {}
    })
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct EpcRule {
    id: String,
    key: String,
    values: String,
}

struct EpcEvaluation {
    response: Option<Value>,
    unsupported_keys: Vec<String>,
}

fn collect_zone_interrogation_rules(value: &Value) -> Vec<EpcRule> {
    fn visit(value: &Value, rules: &mut Vec<EpcRule>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if is_interrogation_list_key(key) {
                        let Some(items) = child.as_array() else {
                            continue;
                        };
                        for item in items {
                            let Some(object) = item.as_object() else {
                                continue;
                            };
                            let Some(id) = object_string(object, &["id", "Id"]) else {
                                continue;
                            };
                            let Some(key) = object_string(object, &["key", "Key"]) else {
                                continue;
                            };
                            let values =
                                object_type_string(object, &["values", "Values", "value", "Value"])
                                    .unwrap_or_default();
                            rules.push(EpcRule {
                                id: sanitize_probe_string(&id),
                                key: sanitize_probe_string(&key),
                                values,
                            });
                        }
                    } else {
                        visit(child, rules);
                    }
                }
            }
            Value::Array(items) => {
                for child in items {
                    visit(child, rules);
                }
            }
            _ => {}
        }
    }

    let mut rules = Vec::new();
    visit(value, &mut rules);
    rules
}

fn evaluate_supported_zone_interrogation(value: &Value) -> EpcEvaluation {
    let rules = collect_zone_interrogation_rules(value);
    let mut unsupported_keys: Vec<String> = Vec::new();
    let mut interrogation_info = Map::new();
    for rule in rules {
        let Some((result, evidence)) = evaluate_epc_rule(&rule) else {
            if !unsupported_keys
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&rule.key))
            {
                unsupported_keys.push(rule.key);
            }
            continue;
        };
        interrogation_info.insert(
            rule.id,
            json!({
                "result": result,
                "value": evidence,
            }),
        );
    }
    unsupported_keys.truncate(12);
    if !unsupported_keys.is_empty() {
        return EpcEvaluation {
            response: None,
            unsupported_keys,
        };
    }
    let mut response = minimal_system_interrogation_response();
    response["interrogation_info"] = Value::Object(interrogation_info);
    EpcEvaluation {
        response: Some(response),
        unsupported_keys,
    }
}

fn evaluate_epc_rule(rule: &EpcRule) -> Option<(bool, Vec<String>)> {
    match rule.key.to_ascii_uppercase().as_str() {
        "OSVERSION" => evaluate_os_version_rule(&rule.values).map(|result| (result, Vec::new())),
        "PROCESS" => {
            let process = first_rule_value(&rule.values)?;
            let result = process_is_running(process);
            Some((result, Vec::new()))
        }
        "FILE" => evaluate_file_rule(&rule.values).map(|result| (result, Vec::new())),
        "DIRECTORY" => evaluate_directory_rule(&rule.values).map(|result| (result, Vec::new())),
        "REGISTRY" => evaluate_registry_rule(&rule.values).map(|result| (result, Vec::new())),
        "EQUIPMENTID" => {
            let equipment_id = equipment_id();
            let result = first_rule_value(&rule.values)
                .map(|_| rule_values_match_any(&rule.values, &equipment_id))
                .unwrap_or(true);
            Some((result, vec![equipment_id]))
        }
        "USERDOMAIN" => {
            let domain = env::var("USERDOMAIN")
                .ok()
                .or_else(|| env::var("USERDNSDOMAIN").ok())?;
            Some((rule_values_match_any(&rule.values, &domain), Vec::new()))
        }
        "MACHINEDOMAIN" => {
            let domain = env::var("USERDNSDOMAIN")
                .ok()
                .or_else(|| env::var("USERDOMAIN").ok())
                .or_else(|| env::var("COMPUTERNAME").ok())?;
            Some((rule_values_match_any(&rule.values, &domain), Vec::new()))
        }
        _ => None,
    }
}

fn evaluate_file_rule(values: &str) -> Option<bool> {
    let parts = split_raw_rule_values(values);
    if parts.len() != 1 {
        return None;
    }
    Some(Path::new(&expand_windows_environment(parts[0])).is_file())
}

fn evaluate_directory_rule(values: &str) -> Option<bool> {
    let parts = split_raw_rule_values(values);
    if parts.len() != 1 {
        return None;
    }
    Some(Path::new(&expand_windows_environment(parts[0])).is_dir())
}

fn evaluate_registry_rule(values: &str) -> Option<bool> {
    let parts = split_raw_rule_values(values);
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    let key = parts[0];
    if key.contains('*') || key.contains('?') {
        return None;
    }
    #[cfg(windows)]
    {
        let output = if parts.len() == 1 {
            Command::new("reg").args(["query", key]).output().ok()?
        } else {
            let value_name = parts[1];
            if value_name.contains('*') || value_name.contains('?') {
                return None;
            }
            Command::new("reg")
                .args(["query", key, "/v", value_name])
                .output()
                .ok()?
        };
        Some(output.status.success())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn first_rule_value(values: &str) -> Option<&str> {
    split_rule_values(values)
        .into_iter()
        .find(|value| !value.trim().is_empty())
}

fn split_rule_values(values: &str) -> Vec<&str> {
    values
        .split([',', ';'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

fn split_raw_rule_values(values: &str) -> Vec<&str> {
    values.split(',').map(str::trim).collect()
}

fn equipment_id() -> String {
    env::var("COMPUTERNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "sing-box-tui".to_string())
}

fn expand_windows_environment(value: &str) -> String {
    let mut output = String::new();
    let mut rest = value.trim_matches('"');
    while let Some(start) = rest.find('%') {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('%') else {
            output.push('%');
            output.push_str(after_start);
            return output;
        };
        let name = &after_start[..end];
        output.push_str(&env::var(name).unwrap_or_else(|_| format!("%{name}%")));
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    output
}

fn rule_values_match_any(values: &str, actual: &str) -> bool {
    split_rule_values(values).into_iter().any(|pattern| {
        wildcard_match(pattern, actual)
            || pattern.eq_ignore_ascii_case(actual)
            || actual
                .to_ascii_lowercase()
                .contains(&pattern.to_ascii_lowercase())
    })
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    fn inner(pattern: &[u8], value: &[u8]) -> bool {
        if pattern.is_empty() {
            return value.is_empty();
        }
        if pattern[0] == b'*' {
            return inner(&pattern[1..], value)
                || (!value.is_empty() && inner(pattern, &value[1..]));
        }
        !value.is_empty()
            && (pattern[0] == b'?' || pattern[0].eq_ignore_ascii_case(&value[0]))
            && inner(&pattern[1..], &value[1..])
    }
    inner(pattern.as_bytes(), value.as_bytes())
}

fn evaluate_os_version_rule(values: &str) -> Option<bool> {
    let parts = split_rule_values(values);
    if parts.is_empty() {
        return None;
    }
    let (operator, version) = if parts.len() >= 2 && is_version_operator(parts[0]) {
        (parts[0], parts[1])
    } else {
        ("=", parts[0])
    };
    let expected = parse_version_tuple(version)?;
    let current = current_os_version()?;
    Some(compare_version_tuples(current, expected, operator))
}

fn is_version_operator(value: &str) -> bool {
    matches!(value, ">" | ">=" | "<" | "<=" | "=" | "==" | "!=")
}

fn parse_version_tuple(value: &str) -> Option<(u32, u32, u32, u32)> {
    let mut output = [0_u32; 4];
    for (index, part) in value
        .split('.')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .take(4)
        .enumerate()
    {
        output[index] = part.parse().ok()?;
    }
    Some((output[0], output[1], output[2], output[3]))
}

fn compare_version_tuples(
    current: (u32, u32, u32, u32),
    expected: (u32, u32, u32, u32),
    operator: &str,
) -> bool {
    match operator {
        ">" => current > expected,
        ">=" => current >= expected,
        "<" => current < expected,
        "<=" => current <= expected,
        "!=" => current != expected,
        _ => current == expected,
    }
}

fn current_os_version() -> Option<(u32, u32, u32, u32)> {
    #[cfg(windows)]
    {
        let output = Command::new("cmd").args(["/C", "ver"]).output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let start = text.find(|character: char| character.is_ascii_digit())?;
        let version = text[start..]
            .chars()
            .take_while(|character| character.is_ascii_digit() || *character == '.')
            .collect::<String>();
        parse_version_tuple(&version)
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout))
            .and_then(|version| parse_version_tuple(version.trim()))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

fn process_is_running(process: &str) -> bool {
    let process = process
        .trim_matches('"')
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(process)
        .trim_end_matches(".exe");
    if process.is_empty() {
        return false;
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args([
                "/FI",
                &format!("IMAGENAME eq {process}.exe"),
                "/FO",
                "CSV",
                "/NH",
            ])
            .output()
            .ok()
            .is_some_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .to_ascii_lowercase()
                        .contains(&format!("\"{}.exe\"", process.to_ascii_lowercase()))
            })
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("/bin/ps")
            .args(["-axo", "comm="])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| Path::new(line.trim()).file_name())
                    .filter_map(|name| name.to_str())
                    .map(|name| name.trim_end_matches(".exe"))
                    .any(|name| name.eq_ignore_ascii_case(process))
            })
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

impl SonicwallAuthSession {
    pub(crate) fn logon_endpoint(&self) -> &str {
        self.logon_endpoint
    }

    pub(crate) fn official_logon_status(&self) -> Option<u16> {
        self.official_logon_status
    }

    pub(crate) fn logon_capability(&self) -> SonicwallLogonCapability {
        self.logon_capability
    }

    pub(crate) fn evpn_identity(&self) -> Result<SonicwallEvpnIdentity> {
        let logon_id = self
            .logon_id
            .lock()
            .map_err(|_| anyhow::anyhow!("SonicWall logon token lock was poisoned"))?;
        let refresh_count = logon_id.refresh_count;
        let observation_count = logon_id.observation_count;
        let logon_id = logon_id
            .token
            .as_ref()
            .context("SonicWall logon response did not include a tunnel team id")?;
        let team_token = decode_logon_id(logon_id.expose_secret())?;
        let server = self
            .gateway
            .host_str()
            .context("SonicWall gateway has no host")?
            .to_string();
        let port = self
            .gateway
            .port_or_known_default()
            .context("SonicWall gateway has no HTTPS port")?;
        Ok(SonicwallEvpnIdentity {
            server,
            port,
            team_token: SonicwallTeamToken(team_token),
            logon_id_refresh_count: refresh_count,
            logon_id_observation_count: observation_count,
        })
    }

    pub(crate) async fn get_agent_info(&self) -> Result<SonicwallAuthStep> {
        self.get_step("agentinfo").await
    }

    pub(crate) async fn post_agent_info(&self, agent_info: &Value) -> Result<SonicwallAuthStep> {
        self.post_step("agentinfo", agent_info).await
    }

    pub(crate) async fn authenticate(
        &self,
        button: &str,
        replies: &[PrivateAccessSecret],
    ) -> Result<SonicwallAuthStep> {
        #[derive(Serialize)]
        struct AuthenticationReply<'a> {
            button: &'a str,
            replies: Vec<&'a str>,
        }

        let payload = AuthenticationReply {
            button,
            replies: replies
                .iter()
                .map(PrivateAccessSecret::expose_secret)
                .collect(),
        };
        self.post_step("authenticate", &payload).await
    }

    pub(crate) async fn activate_connect_tunnel_agent(&self) -> Result<SonicwallAgentActivation> {
        const ENDPOINT: &str = "__api__/config/agents/ConnectTunnel";

        if let Some(value) = self.try_get_gateway_json(ENDPOINT).await? {
            self.capture_logon_id(&value)?;
            return Ok(SonicwallAgentActivation {
                endpoint: Some(ENDPOINT),
            });
        }
        Ok(SonicwallAgentActivation { endpoint: None })
    }

    pub(crate) async fn probe_system_interrogation(
        &self,
    ) -> Result<SonicwallSystemInterrogationProbe> {
        self.probe_interrogation_endpoint("interrogation").await
    }

    async fn probe_interrogation_endpoint(
        &self,
        endpoint: &'static str,
    ) -> Result<SonicwallSystemInterrogationProbe> {
        let Some(value) = self.try_get_session_json(endpoint).await? else {
            return Ok(SonicwallSystemInterrogationProbe {
                endpoint: None,
                zone_count: None,
                zone_keys: Vec::new(),
                unsupported_zone_keys: Vec::new(),
                posted_minimal_response: false,
                is_ct_allow: None,
                zone_command: None,
                zone_type: None,
            });
        };
        self.capture_logon_id(&value)?;
        let zone_count = count_zone_interrogation_items(&value);
        let zone_keys = collect_zone_interrogation_keys(&value);
        if zone_count == 0 {
            let response = self
                .post_session_json(endpoint, &minimal_system_interrogation_response())
                .await?;
            self.capture_logon_id(&response)?;
            return Ok(SonicwallSystemInterrogationProbe {
                endpoint: Some(endpoint),
                zone_count: Some(zone_count),
                zone_keys,
                unsupported_zone_keys: Vec::new(),
                posted_minimal_response: true,
                is_ct_allow: find_bool_key(&response, &["is_ct_allow", "isCtAllow"]),
                zone_command: find_string_key(&response, &["zoneCommand", "ZoneCommand"])
                    .map(sanitize_probe_string),
                zone_type: find_string_key(&response, &["zoneType", "ZoneType"])
                    .map(sanitize_probe_string),
            });
        }
        let evaluation = evaluate_supported_zone_interrogation(&value);
        if let Some(payload) = evaluation.response {
            let response = self.post_session_json(endpoint, &payload).await?;
            self.capture_logon_id(&response)?;
            return Ok(SonicwallSystemInterrogationProbe {
                endpoint: Some(endpoint),
                zone_count: Some(zone_count),
                zone_keys,
                unsupported_zone_keys: Vec::new(),
                posted_minimal_response: true,
                is_ct_allow: find_bool_key(&response, &["is_ct_allow", "isCtAllow"]),
                zone_command: find_string_key(&response, &["zoneCommand", "ZoneCommand"])
                    .map(sanitize_probe_string),
                zone_type: find_string_key(&response, &["zoneType", "ZoneType"])
                    .map(sanitize_probe_string),
            });
        }
        Ok(SonicwallSystemInterrogationProbe {
            endpoint: Some(endpoint),
            zone_count: Some(zone_count),
            zone_keys,
            unsupported_zone_keys: evaluation.unsupported_keys,
            posted_minimal_response: false,
            is_ct_allow: None,
            zone_command: None,
            zone_type: None,
        })
    }

    pub(crate) async fn probe_license_state(&self) -> Result<SonicwallLicenseProbe> {
        const ENDPOINT: &str = "license";
        let Some(value) = self.try_get_session_json(ENDPOINT).await? else {
            return Ok(SonicwallLicenseProbe {
                endpoint: None,
                licensed: None,
                destroy_connections: None,
                status: None,
            });
        };
        self.capture_logon_id(&value)?;
        Ok(SonicwallLicenseProbe {
            endpoint: Some(ENDPOINT),
            licensed: find_bool_key(&value, &["licensed", "Licensed"]),
            destroy_connections: find_bool_key(
                &value,
                &[
                    "destroy_connections",
                    "destroyConnections",
                    "DestroyConnections",
                ],
            ),
            status: find_string_key(&value, &["status", "Status"]).map(sanitize_probe_string),
        })
    }

    pub(crate) async fn probe_connection_state(&self) -> Result<SonicwallConnectionStateProbe> {
        const ENDPOINT: &str = "state";
        let Some(value) = self.try_get_session_json(ENDPOINT).await? else {
            return Ok(SonicwallConnectionStateProbe {
                endpoint: None,
                alpns_supported: None,
                tunnel_protocol_negotiation: None,
                zone_type: None,
            });
        };
        self.capture_logon_id(&value)?;
        Ok(SonicwallConnectionStateProbe {
            endpoint: Some(ENDPOINT),
            alpns_supported: find_bool_key(&value, &["ALPNSupported", "alpnsSupported"]),
            tunnel_protocol_negotiation: find_bool_key(
                &value,
                &["tunnelProtocolNegotiation", "TunnelProtocolNegotiation"],
            ),
            zone_type: find_string_key(&value, &["zoneType", "ZoneType"])
                .map(sanitize_probe_string),
        })
    }

    async fn try_get_gateway_json(&self, path: &str) -> Result<Option<Value>> {
        let url = self
            .gateway
            .join(path)
            .with_context(|| format!("failed to resolve SonicWall gateway resource {path}"))?;
        self.try_get_url_json(url, path).await
    }

    async fn try_get_session_json(&self, suffix: &'static str) -> Result<Option<Value>> {
        let url = self.session_endpoint(suffix)?;
        self.try_get_url_json(url, suffix).await
    }

    async fn try_get_url_json(&self, url: Url, label: &str) -> Result<Option<Value>> {
        let response = self
            .client
            .get(url)
            .header(ACCEPT, SONICWALL_ACCEPT)
            .send()
            .await
            .map_err(|error| {
                sonicwall_request_error(error, format!("failed to GET SonicWall resource {label}"))
            })?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let value = decode_json_response(response, "read session resource").await?;
        Ok(Some(value))
    }

    async fn post_session_json<T>(&self, suffix: &str, payload: &T) -> Result<Value>
    where
        T: Serialize + ?Sized,
    {
        let url = self.session_endpoint(suffix)?;
        let response = self
            .client
            .post(url)
            .header(ACCEPT, SONICWALL_ACCEPT)
            .json(payload)
            .send()
            .await
            .map_err(|error| {
                sonicwall_request_error(
                    error,
                    format!("failed to POST SonicWall session resource {suffix}"),
                )
            })?;
        decode_json_response(response, "post session resource").await
    }

    pub(crate) async fn close(self) -> Result<()> {
        let response = self
            .client
            .delete(self.location.clone())
            .send()
            .await
            .map_err(|error| {
                sonicwall_request_error(error, "failed to delete SonicWall logon session")
            })?;
        if !response.status().is_success() && response.status().as_u16() != 404 {
            bail!(
                "SonicWall logon cleanup returned HTTP {}",
                response.status().as_u16()
            );
        }
        Ok(())
    }

    async fn get_step(&self, suffix: &str) -> Result<SonicwallAuthStep> {
        let url = self.session_endpoint(suffix)?;
        let response =
            interactive_auth_request(self.client.get(url).header(ACCEPT, SONICWALL_ACCEPT))
                .send()
                .await
                .map_err(|error| {
                    sonicwall_request_error(
                        error,
                        format!("failed to GET SonicWall session resource {suffix}"),
                    )
                })?;
        let value = decode_json_response(response, "read authentication session").await?;
        self.capture_logon_id(&value)?;
        Ok(parse_auth_step(value))
    }

    async fn post_step<T>(&self, suffix: &str, payload: &T) -> Result<SonicwallAuthStep>
    where
        T: Serialize + ?Sized,
    {
        let url = self.session_endpoint(suffix)?;
        let response = interactive_auth_request(
            self.client
                .post(url)
                .header(ACCEPT, SONICWALL_ACCEPT)
                .json(payload),
        )
        .send()
        .await
        .map_err(|error| {
            sonicwall_request_error(
                error,
                format!("failed to POST SonicWall session resource {suffix}"),
            )
        })?;
        let value = decode_json_response(response, "advance authentication session").await?;
        self.capture_logon_id(&value)?;
        Ok(parse_auth_step(value))
    }

    fn session_endpoint(&self, suffix: &str) -> Result<Url> {
        let base = self.location.as_str().trim_end_matches('/');
        let candidate = format!("{base}/{}", suffix.trim_start_matches('/'));
        resolve_same_origin_location(&self.gateway, &candidate)
    }

    fn capture_logon_id(&self, value: &Value) -> Result<()> {
        if let Some(logon_id) = find_latest_logon_id(value) {
            let mut current = self
                .logon_id
                .lock()
                .map_err(|_| anyhow::anyhow!("SonicWall logon token lock was poisoned"))?;
            current.observation_count = current.observation_count.saturating_add(1);
            let changed = current
                .token
                .as_ref()
                .is_none_or(|current| current.expose_secret() != logon_id);
            if changed {
                current.refresh_count = current.refresh_count.saturating_add(1);
            }
            current.token = Some(PrivateAccessSecret::new(logon_id));
        }
        Ok(())
    }
}

fn interactive_auth_request(request: RequestBuilder) -> RequestBuilder {
    // Authentication responses may leave the TUI waiting on human input for minutes. The
    // cookie-backed SonicWall session must survive that pause, but its proxy TCP connection must
    // not: sing-box can switch or recycle the selected outbound while the form is open. Closing
    // this HTTP/1.1 connection after every interactive step prevents the next POST from reusing a
    // stale CONNECT/TLS stream without shortening the user's authentication session.
    request.header(CONNECTION, "close")
}

fn decode_logon_id(value: &str) -> Result<[u8; 16]> {
    let bytes = value.as_bytes();
    if bytes.len() != 32 {
        bail!("SonicWall logon team id must contain exactly 32 hexadecimal characters");
    }
    let mut output = [0_u8; 16];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = decode_hex_nibble(pair[0])
            .context("SonicWall logon team id contains a non-hexadecimal character")?;
        let low = decode_hex_nibble(pair[1])
            .context("SonicWall logon team id contains a non-hexadecimal character")?;
        output[index] = (high << 4) | low;
    }
    Ok(output)
}

fn decode_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn find_latest_logon_id(value: &Value) -> Option<String> {
    fn visit(value: &Value, latest: &mut Option<String>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if key.eq_ignore_ascii_case("logonid") {
                        if let Some(candidate) = child.as_str() {
                            if decode_logon_id(candidate).is_ok() {
                                *latest = Some(candidate.to_string());
                            }
                        }
                    }
                    visit(child, latest);
                }
            }
            Value::Array(items) => {
                for child in items {
                    visit(child, latest);
                }
            }
            _ => {}
        }
    }

    let mut latest = None;
    visit(value, &mut latest);
    latest
}

fn sonicwall_request_error(
    error: reqwest::Error,
    context: impl std::fmt::Display + Send + Sync + 'static,
) -> anyhow::Error {
    anyhow::Error::new(error.without_url()).context(context)
}

async fn decode_json_response(response: reqwest::Response, action: &str) -> Result<Value> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .with_context(|| format!("failed to read SonicWall {action} response"))?;
    if !status.is_success() {
        bail!("SonicWall {action} returned HTTP {}", status.as_u16());
    }
    decode_json_body(&body).with_context(|| format!("SonicWall {action} returned invalid JSON"))
}

fn decode_json_body(body: &[u8]) -> Result<Value> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice::<Value>(body).context("response body is not JSON")
}

fn normalize_gateway_url(gateway: &str) -> Result<Url> {
    let raw = gateway.trim();
    if raw.is_empty() {
        bail!("SonicWall gateway cannot be empty");
    }
    let raw = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let mut url = Url::parse(&raw).context("invalid SonicWall gateway URL")?;
    if url.scheme() != "https" {
        bail!("SonicWall gateway must use HTTPS");
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        bail!("SonicWall gateway URL must contain only an HTTPS origin");
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn resolve_same_origin_location(gateway: &Url, location: &str) -> Result<Url> {
    let candidate = gateway
        .join(location)
        .context("invalid SonicWall session location")?;
    if candidate.scheme() != "https" || candidate.origin() != gateway.origin() {
        bail!("SonicWall session location escaped the configured HTTPS origin");
    }
    Ok(candidate)
}

fn parse_auth_step(value: Value) -> SonicwallAuthStep {
    if recursively_has_authenticated(&value) {
        return SonicwallAuthStep::Authenticated;
    }
    if let Some(object) = find_challenge_object(&value) {
        return SonicwallAuthStep::Challenge(parse_challenge(object));
    }
    SonicwallAuthStep::Continue
}

fn parse_challenge(object: &Map<String, Value>) -> SonicwallAuthChallenge {
    let title = object_string(object, &["title", "Title"]).unwrap_or_default();
    let message = object_message(object);
    let fields = object
        .get("fields")
        .or_else(|| object.get("Fields"))
        .and_then(Value::as_array)
        .map(|fields| fields.iter().enumerate().map(parse_auth_field).collect())
        .unwrap_or_default();
    let buttons = object
        .get("buttons")
        .or_else(|| object.get("Buttons"))
        .and_then(Value::as_array)
        .map(|buttons| buttons.iter().filter_map(parse_button).collect())
        .unwrap_or_else(|| vec!["ok".to_string()]);
    SonicwallAuthChallenge {
        title,
        message,
        fields,
        buttons,
    }
}

fn parse_auth_field((index, value): (usize, &Value)) -> PrivateAccessAuthField {
    let object = value.as_object();
    let id = object
        .and_then(|object| object_string(object, &["id", "name", "Id", "Name"]))
        .unwrap_or_else(|| format!("reply-{index}"));
    let label = object
        .and_then(|object| {
            object_string(
                object,
                &["prompt", "text", "label", "Prompt", "Text", "Label"],
            )
        })
        .unwrap_or_else(|| id.clone());
    let kind = object
        .and_then(|object| object_type_string(object, &["type", "Type"]))
        .unwrap_or_else(|| "text".to_string());
    let sensitive = object
        .and_then(|object| object_bool(object, &["SecurePrompt", "securePrompt", "sensitive"]))
        .unwrap_or_else(|| {
            let kind = kind.to_ascii_lowercase();
            kind.contains("password") || kind.contains("passcode") || kind.contains("token")
        });
    let required = object
        .and_then(|object| object_bool(object, &["required", "Required"]))
        .unwrap_or(true);
    let options = object
        .and_then(|object| object.get("options").or_else(|| object.get("Options")))
        .and_then(Value::as_array)
        .map(|options| options.iter().filter_map(parse_auth_option).collect())
        .unwrap_or_default();
    PrivateAccessAuthField {
        id,
        label,
        kind,
        sensitive,
        required,
        options,
    }
}

fn parse_auth_option(value: &Value) -> Option<PrivateAccessAuthOption> {
    if let Some(value) = value.as_str() {
        return Some(PrivateAccessAuthOption {
            value: value.to_string(),
            label: value.to_string(),
        });
    }
    let object = value.as_object()?;
    let value = object_string(object, &["value", "id", "Value", "Id"])?;
    let label =
        object_string(object, &["label", "text", "Label", "Text"]).unwrap_or_else(|| value.clone());
    Some(PrivateAccessAuthOption { value, label })
}

fn parse_button(value: &Value) -> Option<String> {
    value.as_str().map(ToOwned::to_owned).or_else(|| {
        value.as_object().and_then(|object| {
            object_string(object, &["value", "name", "id", "Value", "Name", "Id"])
        })
    })
}

fn parse_realms(value: &Value) -> Vec<SonicwallRealm> {
    fn parse_realm_object(object: &Map<String, Value>) -> Option<SonicwallRealm> {
        let name = object_string(object, &["name", "Name", "displayName"])?;
        let method = object
            .get("method")
            .or_else(|| object.get("Method"))
            .and_then(Value::as_i64);
        Some(SonicwallRealm { name, method })
    }

    fn visit(value: &Value, realms: &mut Vec<SonicwallRealm>) {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    if key.eq_ignore_ascii_case("realms") {
                        if let Some(items) = child.as_array() {
                            for item in items {
                                if let Some(object) = item.as_object() {
                                    if let Some(realm) = parse_realm_object(object) {
                                        realms.push(realm);
                                    }
                                }
                            }
                        }
                    } else {
                        visit(child, realms);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    if let Some(object) = item.as_object() {
                        if let Some(realm) = parse_realm_object(object) {
                            realms.push(realm);
                            continue;
                        }
                    }
                    visit(item, realms);
                }
            }
            _ => {}
        }
    }

    let mut realms = Vec::new();
    visit(value, &mut realms);
    realms
}

fn recursively_has_authenticated(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (key.eq_ignore_ascii_case("authenticated") && value.as_bool() == Some(true))
                || recursively_has_authenticated(value)
        }),
        Value::Array(items) => items.iter().any(recursively_has_authenticated),
        _ => false,
    }
}

fn find_challenge_object(value: &Value) -> Option<&Map<String, Value>> {
    match value {
        Value::Object(object) => {
            let fields = object
                .get("fields")
                .or_else(|| object.get("Fields"))
                .and_then(Value::as_array);
            if fields.is_some() {
                return Some(object);
            }
            object.values().find_map(find_challenge_object)
        }
        Value::Array(items) => items.iter().find_map(find_challenge_object),
        _ => None,
    }
}

fn object_string(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
}

fn object_bool(object: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(Value::as_bool)
}

fn find_bool_key(value: &Value, keys: &[&str]) -> Option<bool> {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(Value::as_bool) {
                    return Some(value);
                }
            }
            object.values().find_map(|value| find_bool_key(value, keys))
        }
        Value::Array(items) => items.iter().find_map(|value| find_bool_key(value, keys)),
        _ => None,
    }
}

fn sanitize_probe_string(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect()
}

fn object_type_string(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    let value = keys.iter().find_map(|key| object.get(*key))?;
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => {
            let values = values.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            if values.is_empty() {
                None
            } else {
                Some(values.join(" "))
            }
        }
        _ => None,
    }
}

fn object_message(object: &Map<String, Value>) -> String {
    let Some(value) = object
        .get("message")
        .or_else(|| object.get("messages"))
        .or_else(|| object.get("Message"))
        .or_else(|| object.get("Messages"))
    else {
        return String::new();
    };
    match value {
        Value::String(message) => message.clone(),
        Value::Array(messages) => messages
            .iter()
            .filter_map(|message| {
                message.as_str().map(ToOwned::to_owned).or_else(|| {
                    message.as_object().and_then(|object| {
                        object_string(object, &["text", "message", "Text", "Message"])
                    })
                })
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn find_string_key<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(Value::as_str) {
                    return Some(value);
                }
            }
            object
                .values()
                .find_map(|value| find_string_key(value, keys))
        }
        Value::Array(items) => items.iter().find_map(|value| find_string_key(value, keys)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use reqwest::Url;
    use serde_json::json;

    use super::{
        SONICWALL_HTTP_POOL_IDLE_TIMEOUT, SonicwallAuthClient, SonicwallAuthStep,
        collect_zone_interrogation_keys, compare_version_tuples, count_zone_interrogation_items,
        decode_logon_id, default_agent_info, evaluate_supported_zone_interrogation,
        find_latest_logon_id, minimal_system_interrogation_response, normalize_gateway_url,
        parse_auth_step, parse_realms, parse_version_tuple, resolve_same_origin_location,
        wildcard_match,
    };

    #[test]
    fn gateway_is_https_and_normalized_to_origin() {
        let url = normalize_gateway_url("vpn.example.com/path?query=1").expect("URL is valid");
        assert_eq!(url.as_str(), "https://vpn.example.com/");
        assert!(normalize_gateway_url("http://vpn.example.com").is_err());
        assert!(normalize_gateway_url("https://user@vpn.example.com").is_err());
    }

    #[test]
    fn auth_client_supports_explicit_direct_and_proxy_transports() {
        SonicwallAuthClient::new("vpn.example.com", true, None)
            .expect("explicit direct client builds");
        SonicwallAuthClient::new("vpn.example.com", true, Some("127.0.0.1:6780"))
            .expect("explicit proxy client builds");
        assert!(
            SonicwallAuthClient::new("vpn.example.com", true, Some("http://[invalid")).is_err()
        );
    }

    #[test]
    fn auth_client_evicts_idle_connections_without_expiring_the_cookie_session() {
        assert_eq!(SONICWALL_HTTP_POOL_IDLE_TIMEOUT.as_secs(), 15);
        assert!(SONICWALL_HTTP_POOL_IDLE_TIMEOUT.as_secs() < 30);
    }

    #[test]
    fn interactive_authentication_steps_do_not_reuse_proxy_connections() {
        let client = reqwest::Client::new();
        let request = super::interactive_auth_request(
            client.post("https://vpn.example.com/__api__/logon/session/authenticate"),
        )
        .build()
        .expect("request builds");
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::CONNECTION)
                .and_then(|value| value.to_str().ok()),
            Some("close")
        );
    }

    #[test]
    fn logon_location_must_remain_on_gateway_origin() {
        let gateway = Url::parse("https://vpn.example.com/").expect("URL is valid");
        let location = resolve_same_origin_location(&gateway, "/__api__/logon/abc")
            .expect("same-origin location is accepted");
        assert_eq!(
            location.as_str(),
            "https://vpn.example.com/__api__/logon/abc"
        );
        assert!(
            resolve_same_origin_location(&gateway, "https://attacker.example/logon/abc").is_err()
        );
        assert!(
            resolve_same_origin_location(&gateway, "http://vpn.example.com/logon/abc").is_err()
        );
    }

    #[test]
    fn logon_id_decodes_to_the_sixteen_byte_team_token() {
        let decoded = decode_logon_id("00112233445566778899AABBCCDDEEFF").unwrap();
        assert_eq!(
            decoded,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );
        assert!(decode_logon_id("not-a-team-id").is_err());
    }

    #[test]
    fn latest_valid_logon_id_is_taken_from_nested_authentication_responses() {
        let response = json!({
            "logonid": "00112233445566778899AABBCCDDEEFF",
            "authenticator": {
                "logonId": "not-a-token",
                "nested": [
                    { "LogonId": "112233445566778899AABBCCDDEEFF00" }
                ]
            }
        });
        assert_eq!(
            find_latest_logon_id(&response).as_deref(),
            Some("112233445566778899AABBCCDDEEFF00")
        );
    }

    #[test]
    fn default_agent_info_matches_modern_connect_tunnel_micro_interrogation() {
        let agent_info = default_agent_info();
        assert_eq!(agent_info["mac"], json!(false));
        assert_eq!(agent_info["osxlion"], json!(false));
        assert_eq!(agent_info["win"], json!(true));
        assert_eq!(agent_info["platform"], json!("Windows"));
        assert_eq!(agent_info["modernTunnelClient"], json!(true));
        assert_eq!(agent_info["mobileConnect"], json!(false));
        assert_eq!(agent_info["nonInteractiveClient"], json!(false));
        assert_eq!(agent_info["captchaCapable"], json!(false));
        assert_eq!(agent_info["userAgent"], json!("SonicWall Connect Tunnel"));
        assert_eq!(agent_info["userLocale"], json!("zh"));
    }

    #[test]
    fn system_interrogation_counts_zone_rules_before_posting_minimal_response() {
        let empty = json!({ "zone_interrogation_list": [] });
        let nested = json!({
            "interrogation": {
                "zone_interrogation_list": [
                    { "id": 1, "key": "OSVERSION" },
                    { "id": 2, "key": "PROCESS" }
                ],
                "micro_interrogation_list": [
                    { "id": 3, "key": "EQUIPMENTID" }
                ]
            }
        });
        assert_eq!(count_zone_interrogation_items(&empty), 0);
        assert_eq!(count_zone_interrogation_items(&nested), 3);
        assert_eq!(
            collect_zone_interrogation_keys(&nested),
            vec![
                "OSVERSION".to_string(),
                "PROCESS".to_string(),
                "EQUIPMENTID".to_string()
            ]
        );
    }

    #[test]
    fn minimal_system_interrogation_response_uses_connect_tunnel_client_type() {
        let response = minimal_system_interrogation_response();
        assert_eq!(response["type"], json!("EPC"));
        assert_eq!(response["client_info"]["client_type"], json!("CT"));
        assert_eq!(response["interrogation_info"], json!({}));
    }

    #[test]
    fn epc_evaluator_helpers_compare_versions_and_wildcards() {
        assert_eq!(parse_version_tuple("10.0.26100"), Some((10, 0, 26100, 0)));
        assert!(compare_version_tuples(
            (10, 0, 26100, 0),
            (10, 0, 19045, 0),
            ">="
        ));
        assert!(wildcard_match("HUNDSUN*", "HundsunDomain"));
        assert!(!wildcard_match("HUNDSUN?", "HundsunDomain"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn epc_evaluator_reports_macos_version_rules() {
        let interrogation = json!({
            "zone_interrogation_list": [
                { "id": "mac-version", "key": "OSVERSION", "values": ">=,0" }
            ]
        });
        let evaluation = evaluate_supported_zone_interrogation(&interrogation);
        let response = evaluation
            .response
            .expect("macOS version rule is supported");
        assert_eq!(
            response["interrogation_info"]["mac-version"]["result"],
            json!(true)
        );
        assert!(evaluation.unsupported_keys.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn epc_evaluator_reports_running_macos_processes() {
        let executable = std::env::current_exe().expect("current executable is available");
        let process = executable
            .file_name()
            .and_then(|name| name.to_str())
            .expect("test executable has a UTF-8 name");
        let interrogation = json!({
            "zone_interrogation_list": [
                { "id": "current-process", "key": "PROCESS", "values": process }
            ]
        });
        let evaluation = evaluate_supported_zone_interrogation(&interrogation);
        let response = evaluation
            .response
            .expect("macOS process rule is supported");
        assert_eq!(
            response["interrogation_info"]["current-process"]["result"],
            json!(true)
        );
    }

    #[test]
    fn epc_evaluator_posts_when_all_zone_keys_are_safely_supported() {
        let interrogation = json!({
            "zone_interrogation_list": [
                { "id": 1, "key": "FILE", "values": "Cargo.toml" },
                { "id": 2, "key": "DIRECTORY", "values": "." },
                { "id": 3, "key": "EQUIPMENTID", "values": "" }
            ]
        });
        let evaluation = evaluate_supported_zone_interrogation(&interrogation);
        let response = evaluation.response.expect("safe rules can be posted");
        assert!(evaluation.unsupported_keys.is_empty());
        assert_eq!(response["interrogation_info"]["1"]["result"], json!(true));
        assert_eq!(response["interrogation_info"]["2"]["result"], json!(true));
        assert_eq!(response["interrogation_info"]["3"]["result"], json!(true));
        assert!(
            response["interrogation_info"]["3"]["value"][0]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }

    #[test]
    fn epc_evaluator_refuses_unsupported_zone_keys_instead_of_faking_results() {
        let interrogation = json!({
            "zone_interrogation_list": [
                { "id": 1, "key": "REGISTRY", "values": "HKLM\\Software\\Vendor,ValueName,>,1" },
                { "id": 2, "key": "OPSWATAV", "values": "" }
            ]
        });
        let evaluation = evaluate_supported_zone_interrogation(&interrogation);
        assert!(evaluation.response.is_none());
        assert_eq!(
            evaluation.unsupported_keys,
            vec!["REGISTRY".to_string(), "OPSWATAV".to_string()]
        );
    }

    #[test]
    fn dynamic_authenticator_is_converted_without_hardcoded_field_count() {
        let response = json!({
            "authenticator": {
                "authenticated": false,
                "title": "Hundsun",
                "messages": [{ "text": "Enter credentials" }],
                "fields": [
                    { "prompt": "Domain account", "type": ["text", "is-username"] },
                    { "prompt": "Domain password", "type": ["password", "is-password"] },
                    { "prompt": "Dynamic code", "type": ["password"] }
                ],
                "buttons": [{ "name": "ok" }, { "name": "cancel" }]
            }
        });

        let SonicwallAuthStep::Challenge(challenge) = parse_auth_step(response) else {
            panic!("expected authentication challenge");
        };
        assert_eq!(challenge.title, "Hundsun");
        assert_eq!(challenge.fields.len(), 3);
        assert!(!challenge.fields[0].sensitive);
        assert!(challenge.fields[1].sensitive);
        assert!(challenge.fields[2].sensitive);
        assert_eq!(challenge.buttons, ["ok", "cancel"]);
    }

    #[test]
    fn authenticated_response_is_not_misclassified_as_another_prompt() {
        let response = json!({
            "authenticator": {
                "authenticated": true,
                "fields": []
            }
        });
        assert!(matches!(
            parse_auth_step(response),
            SonicwallAuthStep::Authenticated
        ));
    }

    #[test]
    fn realms_are_discovered_from_nested_public_config() {
        let response = json!({
            "config": {
                "realms": [
                    { "name": "Hundsun", "method": 144 },
                    { "name": "EquipmentID", "method": 151 }
                ]
            }
        });
        let realms = parse_realms(&response);
        assert_eq!(realms.len(), 2);
        assert_eq!(realms[0].name, "Hundsun");
        assert_eq!(realms[0].method, Some(144));
    }

    #[test]
    fn realms_are_discovered_from_direct_realms_array() {
        let response = json!([
            { "name": "Hundsun", "method": 144 },
            { "displayName": "EquipmentID", "Method": 151 }
        ]);
        let realms = parse_realms(&response);
        assert_eq!(realms.len(), 2);
        assert_eq!(realms[0].name, "Hundsun");
        assert_eq!(realms[1].name, "EquipmentID");
        assert_eq!(realms[1].method, Some(151));
    }

    #[test]
    fn empty_success_body_decodes_as_json_null() {
        assert_eq!(super::decode_json_body(b"  \r\n").unwrap(), json!(null));
    }
}
