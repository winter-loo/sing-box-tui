use std::collections::BTreeSet;
use std::fs;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::atomic_file::write_atomic;
use crate::defaults::{
    AUTO_SELECTOR_TAG_ALIASES, BLOCK_TAG_ALIASES, DEFAULT_AD_BLOCK_SELECTOR_TAG,
    DEFAULT_AUTO_SELECTOR_TAG, DEFAULT_BLOCK_TAG, DEFAULT_BYPASS_RULE_SET_PATH,
    DEFAULT_BYPASS_RULE_SET_TAG, DEFAULT_DELAY_TEST_URL, DEFAULT_DIRECT_TAG, DEFAULT_LOCAL_DNS_TAG,
    DEFAULT_REMOTE_DNS_TAG, DEFAULT_SELECTOR_TAG, DIRECT_TAG_ALIASES, SELECTOR_TAG_ALIASES,
    default_clash_api_external_controller,
};

// Tailscale uses RFC6598 CGNAT addresses, which should stay on the overlay.
const CGNAT_OVERLAY_CIDR: &str = "100.64.0.0/10";
const PRIVATE_ACCESS_SYSTEM_DNS_TAG: &str = "private-access-system";
const INTERNET_TUN_INBOUND_TAG: &str = "tun-in";
const TAILSCALE_ENDPOINT_TAG: &str = "ts-ep";
const TAILSCALE_DNS_TAG: &str = "tailscale-dns";
const TAILSCALE_STATE_DIRECTORY: &str = ".local/tailscale-embedded";
const TAILSCALE_IPV6_CIDR: &str = "fd7a:115c:a1e0::/48";
// AliDNS alternate address, used as the directly reachable remote DoT default.
const DEFAULT_REMOTE_DNS_SERVER: &str = "223.6.6.6";
const LEGACY_REMOTE_DNS_SERVER: &str = "8.8.8.8";
const DEFAULT_REMOTE_DNS_PORT: u64 = 853;

// Every config editor performs a read-modify-write cycle. Atomic replacement protects readers
// from partial files, while this process-wide lock prevents concurrent editors from committing
// changes derived from the same stale snapshot and silently overwriting each other.
static CONFIG_MUTATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_config_mutation() -> MutexGuard<'static, ()> {
    CONFIG_MUTATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// China split-routing rule-set tags. These are the binary rule-sets the China IP routing
// toggle adds or removes, distinct from the AdGuard ad-block rule-set.
const CHINA_GEOIP_RULE_SET_TAG: &str = "geoip-cn";
const CHINA_GEOSITE_CN_RULE_SET_TAG: &str = "geosite-cn";
const CHINA_GEOSITE_GEOLOCATION_CN_RULE_SET_TAG: &str = "geosite-geolocation-cn";
const CHINA_GEOSITE_GEOLOCATION_NOT_CN_RULE_SET_TAG: &str = "geosite-geolocation-!cn";
const CHINA_IP_ROUTING_CLIENT_SUBNET: &str = "114.114.114.114/24";
const CHINA_IP_ROUTING_RULESET_DIR: &str = "sing-box-tui-rulesets";

// Canonical (tag, download URL) pairs for the China split-routing rule-sets. The TUI downloads
// these through the proxy and writes them as local binary rule-sets so sing-box never needs to
// reach GitHub raw at startup (a failed remote download is fatal for sing-box).
pub(crate) const CHINA_IP_ROUTING_RULE_SETS: &[(&str, &str)] = &[
    (
        CHINA_GEOIP_RULE_SET_TAG,
        "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geoip/cn.srs",
    ),
    (
        CHINA_GEOSITE_CN_RULE_SET_TAG,
        "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/cn.srs",
    ),
    (
        CHINA_GEOSITE_GEOLOCATION_CN_RULE_SET_TAG,
        "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/geolocation-cn.srs",
    ),
    (
        CHINA_GEOSITE_GEOLOCATION_NOT_CN_RULE_SET_TAG,
        "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/geolocation-!cn.srs",
    ),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HillstoneRouteOptions {
    pub(crate) target: Ipv4Addr,
    pub(crate) proxy: SocketAddrV4,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HillstoneRouteTableOptions {
    pub(crate) cidrs: Vec<String>,
    pub(crate) proxy: SocketAddrV4,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrivateAccessRouteTableOptions {
    pub(crate) profile_id: String,
    pub(crate) cidrs: Vec<String>,
    pub(crate) domains: Vec<String>,
    pub(crate) domain_suffixes: Vec<String>,
    pub(crate) previous_cidrs: Vec<String>,
    pub(crate) previous_domains: Vec<String>,
    pub(crate) previous_domain_suffixes: Vec<String>,
    pub(crate) carrier_domains: Vec<String>,
    pub(crate) proxy: Option<SocketAddrV4>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DefaultConfigOptions {
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RouteAutoDetectInterfaceState {
    RouteMissing,
    FieldMissing,
    Disabled,
    Enabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InternetTunModeUpdate {
    pub(crate) changed: bool,
    pub(crate) auto_detect_interface_before_enable: RouteAutoDetectInterfaceState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TunConfigState {
    pub(crate) managed_internet_tun: bool,
    pub(crate) other_tun: bool,
    pub(crate) reserved_tag_conflict: bool,
    pub(crate) auto_detect_interface: RouteAutoDetectInterfaceState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TailscaleConfigState {
    pub(crate) enabled: bool,
    pub(crate) tailnet_domain: Option<String>,
    pub(crate) hostname: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TailscaleConfigOptions {
    pub(crate) tailnet_domain: String,
    pub(crate) hostname: Option<String>,
}

impl TunConfigState {
    pub(crate) fn has_any_tun(self) -> bool {
        self.managed_internet_tun || self.other_tun
    }

    pub(crate) fn has_conflicting_tuns(self) -> bool {
        self.managed_internet_tun && self.other_tun
    }
}

pub(crate) fn build_full_config_with_options(
    config_path: &PathBuf,
    imported_nodes: Vec<Value>,
    replace_nodes: bool,
    default_config_options: DefaultConfigOptions,
) -> Result<Value> {
    if config_path.exists() {
        let text = fs::read_to_string(config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut config: Value = parse_sing_box_config_text(&text)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        merge_into_existing_config(&mut config, imported_nodes, replace_nodes)?;
        Ok(config)
    } else {
        Ok(build_default_config_with_options(
            imported_nodes,
            default_config_options,
        ))
    }
}

pub(crate) fn ensure_bypass_rule_set_file_for_config(
    config_path: &Path,
) -> Result<Option<PathBuf>> {
    let config_dir = config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let bypass_path = config_dir.join(DEFAULT_BYPASS_RULE_SET_PATH);
    if bypass_path.exists() {
        return Ok(None);
    }

    let contents = serde_json::to_string_pretty(&json!({
        "version": 1,
        "rules": [],
    }))
    .context("failed to serialize default bypass rule-set")?;
    fs::write(&bypass_path, format!("{contents}\n"))
        .with_context(|| format!("failed to write {}", bypass_path.display()))?;
    Ok(Some(bypass_path))
}

pub(crate) fn parse_sing_box_config_text(text: &str) -> Result<Value> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(strict_error) => {
            // Sing-box accepts JSONC-style operator edits such as comments and trailing commas.
            // Normalizing those cases here lets this tool edit the same config sing-box can run
            // while keeping the written result as portable strict JSON.
            let normalized = normalize_sing_box_jsonc(text);
            serde_json::from_str(&normalized).with_context(|| {
                format!(
                    "strict JSON parse failed ({strict_error}); failed again after normalizing sing-box JSONC"
                )
            })
        }
    }
}

fn normalize_sing_box_jsonc(text: &str) -> String {
    strip_json_trailing_commas(&strip_json_comments(text))
}

fn strip_json_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;

    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    output.push(escaped);
                }
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                output.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                for comment_char in chars.by_ref() {
                    if comment_char == '\n' {
                        output.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for comment_char in chars.by_ref() {
                    if comment_char == '\n' {
                        output.push('\n');
                    }
                    if previous == '*' && comment_char == '/' {
                        break;
                    }
                    previous = comment_char;
                }
            }
            _ => output.push(ch),
        }
    }

    output
}

fn strip_json_trailing_commas(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;

    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    output.push(escaped);
                }
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                output.push(ch);
            }
            ',' => {
                let mut lookahead = chars.clone();
                let next_significant = lookahead.find(|next| !next.is_whitespace());
                if !matches!(next_significant, Some(']' | '}')) {
                    output.push(ch);
                }
            }
            _ => output.push(ch),
        }
    }

    output
}

pub(crate) fn run_hillstone_route_config(
    config_path: &Path,
    output: Option<&PathBuf>,
    write: bool,
    options: HillstoneRouteOptions,
) -> Result<()> {
    run_hillstone_route_table_config(
        config_path,
        output,
        write,
        HillstoneRouteTableOptions {
            cidrs: vec![format!("{}/32", options.target)],
            proxy: options.proxy,
        },
    )
}

pub(crate) fn run_hillstone_route_table_config(
    config_path: &Path,
    output: Option<&PathBuf>,
    write: bool,
    options: HillstoneRouteTableOptions,
) -> Result<()> {
    run_private_access_route_table_config(
        config_path,
        output,
        write,
        PrivateAccessRouteTableOptions {
            profile_id: "hillstone".to_string(),
            cidrs: options.cidrs,
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            previous_cidrs: Vec::new(),
            previous_domains: Vec::new(),
            previous_domain_suffixes: Vec::new(),
            carrier_domains: Vec::new(),
            proxy: Some(options.proxy),
        },
    )
    .map(|_| ())
}

pub(crate) fn run_private_access_route_table_config(
    config_path: &Path,
    output: Option<&PathBuf>,
    write: bool,
    options: PrivateAccessRouteTableOptions,
) -> Result<bool> {
    let _config_guard = lock_config_mutation();
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut config: Value = parse_sing_box_config_text(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let original = config.clone();
    ensure_private_access_route_table(&mut config, options)?;
    let changed = config != original;
    let contents =
        serde_json::to_string_pretty(&config).context("failed to serialize updated config")?;

    if write && changed {
        write_atomic(config_path, format!("{contents}\n").as_bytes())
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }
    if let Some(output) = output {
        write_atomic(output, format!("{contents}\n").as_bytes())
            .with_context(|| format!("failed to write {}", output.display()))?;
    }
    Ok(changed)
}

pub(crate) fn run_private_access_tun_baseline_config(
    config_path: &PathBuf,
    write: bool,
    carrier_domains: &[String],
) -> Result<bool> {
    let _config_guard = lock_config_mutation();
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut config: Value = parse_sing_box_config_text(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let original = config.clone();
    ensure_private_access_tun_baseline(&mut config, carrier_domains)?;
    let changed = config != original;
    if write && changed {
        let contents =
            serde_json::to_string_pretty(&config).context("failed to serialize updated config")?;
        write_atomic(config_path, format!("{contents}\n").as_bytes())
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }
    Ok(changed)
}

pub(crate) fn default_tun_inbound() -> Value {
    json!({
        "type": "tun",
        "tag": INTERNET_TUN_INBOUND_TAG,
        "address": ["172.19.0.1/30"],
        "mtu": 9000,
        "auto_route": true,
        "strict_route": true,
        "stack": "mixed",
        "endpoint_independent_nat": true,
    })
}

/// Reports whether the sing-box config already contains an Internet Proxy TUN inbound.
///
/// This is the sing-box `tun` inbound (system traffic capture) managed by the TUI's
/// Internet Proxy TUN toggle, distinct from the Private Access TUN data-plane helper.
#[cfg(test)]
pub(crate) fn config_has_internet_tun_inbound(config_path: &Path) -> Result<bool> {
    Ok(inspect_tun_config(config_path)?.managed_internet_tun)
}

pub(crate) fn inspect_tun_config(config_path: &Path) -> Result<TunConfigState> {
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config: Value = parse_sing_box_config_text(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    tun_config_state(&config)
}

/// Adds or removes the Internet Proxy TUN inbound in the sing-box config and keeps the
/// outbound-interface policy aligned with that mode. The caller supplies the saved route state
/// when disabling so unrelated user configuration can be restored exactly.
pub(crate) fn set_internet_tun_mode(
    config_path: &Path,
    enable: bool,
    restore_auto_detect_interface: Option<RouteAutoDetectInterfaceState>,
) -> Result<InternetTunModeUpdate> {
    let _config_guard = lock_config_mutation();
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut config: Value = parse_sing_box_config_text(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let tun_state = tun_config_state(&config)?;
    let auto_detect_interface_before_enable = tun_state.auto_detect_interface;
    let has_managed_internet_tun = tun_state.managed_internet_tun;
    let has_other_tun = tun_state.other_tun;
    let has_private_access_baseline = config_has_private_access_tun_baseline(&config);
    if enable && tun_state.reserved_tag_conflict {
        bail!(
            "cannot enable the managed Internet TUN: inbound tag '{INTERNET_TUN_INBOUND_TAG}' is not uniquely available"
        );
    }
    if enable && has_other_tun {
        bail!(
            "cannot enable the managed Internet TUN while another TUN inbound is present; preserve or remove the custom TUN explicitly"
        );
    }
    let original = config.clone();
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    if enable {
        let inbounds = root
            .entry("inbounds")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .context("existing config inbounds must be an array")?;
        if !inbounds.iter().any(is_internet_tun_inbound) {
            inbounds.push(default_tun_inbound());
        }
    } else if let Some(inbounds) = root.get_mut("inbounds") {
        let inbounds = inbounds
            .as_array_mut()
            .context("existing config inbounds must be an array")?;
        if has_managed_internet_tun {
            inbounds.retain(|inbound| !is_internet_tun_inbound(inbound));
        }
    }
    if enable {
        set_internet_tun_auto_detect_interface(root, true)?;
    } else if !has_other_tun {
        // Once the managed inbound is removed, a remaining custom TUN owns the shared route
        // policy. Otherwise restore the managed toggle's rollback state.
        if let Some(state) = restore_auto_detect_interface {
            restore_route_auto_detect_interface(root, state)?;
        } else if has_private_access_baseline {
            // Backward compatibility for state written before the original route preference was
            // persisted. This marker is owned by the managed Private Access baseline.
            set_internet_tun_auto_detect_interface(root, false)?;
        }
    }
    let changed = config != original;
    if changed {
        let contents =
            serde_json::to_string_pretty(&config).context("failed to serialize updated config")?;
        write_atomic(config_path, format!("{contents}\n").as_bytes())
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }
    Ok(InternetTunModeUpdate {
        changed,
        auto_detect_interface_before_enable,
    })
}

fn config_has_internet_tun_inbound_value(config: &Value) -> bool {
    config
        .get("inbounds")
        .and_then(Value::as_array)
        .is_some_and(|inbounds| inbounds.iter().any(is_internet_tun_inbound))
}

fn tun_config_state(config: &Value) -> Result<TunConfigState> {
    Ok(TunConfigState {
        managed_internet_tun: config_has_internet_tun_inbound_value(config),
        other_tun: config_has_other_tun_inbound(config),
        reserved_tag_conflict: config_has_reserved_internet_tun_tag_conflict(config),
        auto_detect_interface: route_auto_detect_interface_state(config)?,
    })
}

fn is_tun_inbound(inbound: &Value) -> bool {
    inbound.get("type").and_then(Value::as_str) == Some("tun")
}

fn is_internet_tun_inbound(inbound: &Value) -> bool {
    is_tun_inbound(inbound)
        && inbound.get("tag").and_then(Value::as_str) == Some(INTERNET_TUN_INBOUND_TAG)
}

fn config_has_other_tun_inbound(config: &Value) -> bool {
    config
        .get("inbounds")
        .and_then(Value::as_array)
        .is_some_and(|inbounds| {
            inbounds
                .iter()
                .any(|inbound| is_tun_inbound(inbound) && !is_internet_tun_inbound(inbound))
        })
}

fn config_has_reserved_internet_tun_tag_conflict(config: &Value) -> bool {
    config
        .get("inbounds")
        .and_then(Value::as_array)
        .is_some_and(|inbounds| {
            let mut reserved = inbounds.iter().filter(|inbound| {
                inbound.get("tag").and_then(Value::as_str) == Some(INTERNET_TUN_INBOUND_TAG)
            });
            reserved
                .next()
                .is_some_and(|first| !is_internet_tun_inbound(first) || reserved.next().is_some())
        })
}

fn config_has_private_access_tun_baseline(config: &Value) -> bool {
    config
        .get("dns")
        .and_then(|dns| dns.get("servers"))
        .and_then(Value::as_array)
        .is_some_and(|servers| {
            servers.iter().any(|server| {
                server.get("tag").and_then(Value::as_str) == Some(PRIVATE_ACCESS_SYSTEM_DNS_TAG)
            })
        })
}

fn route_auto_detect_interface_state(config: &Value) -> Result<RouteAutoDetectInterfaceState> {
    let Some(route) = config.get("route") else {
        return Ok(RouteAutoDetectInterfaceState::RouteMissing);
    };
    let route = route
        .as_object()
        .context("existing config route must be an object")?;
    match route.get("auto_detect_interface") {
        None => Ok(RouteAutoDetectInterfaceState::FieldMissing),
        Some(Value::Bool(false)) => Ok(RouteAutoDetectInterfaceState::Disabled),
        Some(Value::Bool(true)) => Ok(RouteAutoDetectInterfaceState::Enabled),
        Some(_) => bail!("existing config route.auto_detect_interface must be a boolean"),
    }
}

/// `auto_route` sends the system default route through the Internet TUN. The sing-box process
/// must therefore bind its own node/DNS dials to the physical default interface or those dials
/// are captured by the same TUN again. When Internet TUN is off, the managed Private Access
/// baseline deliberately leaves direct dials unbound so its more-specific OS routes can win.
fn set_internet_tun_auto_detect_interface(
    root: &mut Map<String, Value>,
    internet_tun_enabled: bool,
) -> Result<&mut Map<String, Value>> {
    let route = root
        .entry("route")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing config route must be an object")?;
    route.insert(
        "auto_detect_interface".to_string(),
        Value::Bool(internet_tun_enabled),
    );
    Ok(route)
}

fn restore_route_auto_detect_interface(
    root: &mut Map<String, Value>,
    state: RouteAutoDetectInterfaceState,
) -> Result<()> {
    match state {
        RouteAutoDetectInterfaceState::Disabled => {
            set_internet_tun_auto_detect_interface(root, false)?;
        }
        RouteAutoDetectInterfaceState::Enabled => {
            set_internet_tun_auto_detect_interface(root, true)?;
        }
        RouteAutoDetectInterfaceState::FieldMissing => {
            let route = root
                .entry("route")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("existing config route must be an object")?;
            route.remove("auto_detect_interface");
        }
        RouteAutoDetectInterfaceState::RouteMissing => {
            let Some(route) = root.get_mut("route") else {
                return Ok(());
            };
            let route = route
                .as_object_mut()
                .context("existing config route must be an object")?;
            route.remove("auto_detect_interface");
            if route.is_empty() {
                root.remove("route");
            }
        }
    }
    Ok(())
}

fn china_ip_routing_rule_set_tags() -> &'static [&'static str] {
    &[
        CHINA_GEOIP_RULE_SET_TAG,
        CHINA_GEOSITE_CN_RULE_SET_TAG,
        CHINA_GEOSITE_GEOLOCATION_CN_RULE_SET_TAG,
        CHINA_GEOSITE_GEOLOCATION_NOT_CN_RULE_SET_TAG,
    ]
}

fn china_ip_routing_route_rule_tags() -> &'static [&'static str] {
    &[
        CHINA_GEOIP_RULE_SET_TAG,
        CHINA_GEOSITE_CN_RULE_SET_TAG,
        CHINA_GEOSITE_GEOLOCATION_CN_RULE_SET_TAG,
    ]
}

fn china_ip_routing_rule_sets(direct_tag: &str) -> Vec<Value> {
    CHINA_IP_ROUTING_RULE_SETS
        .iter()
        .map(|&(tag, url)| {
            json!({
                "type": "remote",
                "tag": tag,
                "format": "binary",
                "url": url,
                "download_detour": direct_tag,
                "update_interval": "30d",
            })
        })
        .collect()
}

fn china_ip_routing_local_rule_sets(ruleset_dir: &Path) -> Vec<Value> {
    CHINA_IP_ROUTING_RULE_SETS
        .iter()
        .map(|&(tag, _)| {
            json!({
                "type": "local",
                "tag": tag,
                "format": "binary",
                "path": ruleset_dir.join(format!("{tag}.srs")).to_string_lossy().into_owned(),
            })
        })
        .collect()
}

pub(crate) fn china_ip_routing_ruleset_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(CHINA_IP_ROUTING_RULESET_DIR)
}

fn china_ip_routing_route_rules(direct_tag: &str) -> Vec<Value> {
    vec![
        json!({ "rule_set": CHINA_GEOIP_RULE_SET_TAG, "outbound": direct_tag }),
        json!({ "rule_set": CHINA_GEOSITE_CN_RULE_SET_TAG, "outbound": direct_tag }),
        json!({
            "rule_set": CHINA_GEOSITE_GEOLOCATION_CN_RULE_SET_TAG,
            "outbound": direct_tag,
        }),
    ]
}

fn china_ip_routing_dns_rules() -> Vec<Value> {
    vec![
        json!({
            "rule_set": CHINA_GEOSITE_CN_RULE_SET_TAG,
            "server": DEFAULT_LOCAL_DNS_TAG,
        }),
        json!({
            "rule_set": CHINA_GEOSITE_GEOLOCATION_CN_RULE_SET_TAG,
            "server": DEFAULT_LOCAL_DNS_TAG,
        }),
        json!({
            "type": "logical",
            "mode": "and",
            "rules": [
                { "rule_set": CHINA_GEOSITE_GEOLOCATION_NOT_CN_RULE_SET_TAG, "invert": true },
                { "rule_set": CHINA_GEOIP_RULE_SET_TAG }
            ],
            "server": DEFAULT_REMOTE_DNS_TAG,
            "client_subnet": CHINA_IP_ROUTING_CLIENT_SUBNET,
        }),
    ]
}

fn is_direct_clash_mode(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "direct" | "直连")
}

fn is_global_clash_mode(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "global" | "全局")
}

fn is_rule_clash_mode(value: &str) -> bool {
    !is_direct_clash_mode(value) && !is_global_clash_mode(value)
}

fn clash_mode_rule_insert_index(rules: &[Value]) -> usize {
    // Insert China rules before the rule-mode catch-all so they apply in rule mode while the
    // direct/global mode overrides still take precedence. The rule mode is any clash_mode value
    // that is neither direct nor global (covers 规则 / Rule / rule, etc.). If none is found, append
    // at the end rather than before the first clash_mode rule, which would break global mode.
    rules
        .iter()
        .position(|rule| {
            rule.get("clash_mode")
                .and_then(Value::as_str)
                .is_some_and(is_rule_clash_mode)
        })
        .unwrap_or(rules.len())
}

/// Reports whether the sing-box config already has the China split-routing rule-sets enabled.
pub(crate) fn config_has_china_ip_routing(config_path: &Path) -> Result<bool> {
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config: Value = parse_sing_box_config_text(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    Ok(config_has_china_ip_routing_value(&config))
}

fn config_has_china_ip_routing_value(config: &Value) -> bool {
    config
        .get("route")
        .and_then(|route| route.get("rule_set"))
        .and_then(Value::as_array)
        .is_some_and(|rule_sets| {
            rule_sets.iter().any(|rule_set| {
                rule_set.get("tag").and_then(Value::as_str) == Some(CHINA_GEOIP_RULE_SET_TAG)
            })
        })
}

/// Adds or removes the China split-routing rule-sets and rules in the sing-box config,
/// writing the result back in place when it changed. Returns whether the config was modified.
pub(crate) fn set_china_ip_routing(config_path: &Path, enable: bool) -> Result<bool> {
    let _config_guard = lock_config_mutation();
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut config: Value = parse_sing_box_config_text(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let original = config.clone();
    let ruleset_dir = china_ip_routing_ruleset_dir(config_path);
    ensure_china_ip_routing(&mut config, enable, &ruleset_dir)?;
    let changed = config != original;
    if changed {
        let contents =
            serde_json::to_string_pretty(&config).context("failed to serialize updated config")?;
        write_atomic(config_path, format!("{contents}\n").as_bytes())
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }
    Ok(changed)
}

pub(crate) fn inspect_tailscale_config(config_path: &Path) -> Result<TailscaleConfigState> {
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config = parse_sing_box_config_text(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    Ok(tailscale_config_state(&config))
}

pub(crate) fn set_tailscale_config(
    config_path: &Path,
    options: Option<TailscaleConfigOptions>,
) -> Result<bool> {
    let _config_guard = lock_config_mutation();
    let text = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut config = parse_sing_box_config_text(&text)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let original = config.clone();
    ensure_tailscale_config(&mut config, options)?;
    let changed = config != original;
    if changed {
        let contents =
            serde_json::to_string_pretty(&config).context("failed to serialize updated config")?;
        write_atomic(config_path, format!("{contents}\n").as_bytes())
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }
    Ok(changed)
}

fn tailscale_config_state(config: &Value) -> TailscaleConfigState {
    let endpoint = config
        .get("endpoints")
        .and_then(Value::as_array)
        .and_then(|endpoints| {
            endpoints
                .iter()
                .find(|endpoint| is_managed_tailscale_endpoint(endpoint))
        });
    let tailnet_domain = config
        .get("dns")
        .and_then(|dns| dns.get("rules"))
        .and_then(Value::as_array)
        .and_then(|rules| {
            rules
                .iter()
                .find(|rule| is_managed_tailscale_dns_rule(rule))
        })
        .and_then(|rule| rule.get("domain_suffix"))
        .and_then(Value::as_array)
        .and_then(|domains| domains.first())
        .and_then(Value::as_str)
        .map(ToString::to_string);
    TailscaleConfigState {
        enabled: endpoint.is_some(),
        tailnet_domain,
        hostname: endpoint
            .and_then(|endpoint| endpoint.get("hostname"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
    }
}

fn ensure_tailscale_config(
    config: &mut Value,
    options: Option<TailscaleConfigOptions>,
) -> Result<()> {
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;

    if options.is_some() {
        reject_tailscale_dns_tag_collision(root)?;
    }
    remove_managed_tailscale_config(root)?;
    let Some(options) = options else {
        return Ok(());
    };
    let tailnet_domain = options.tailnet_domain.trim().trim_start_matches('.');
    if tailnet_domain.is_empty() {
        bail!("Tailscale tailnet domain cannot be empty");
    }

    let endpoints = root
        .entry("endpoints")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config endpoints must be an array")?;
    if endpoints
        .iter()
        .any(|endpoint| endpoint.get("tag").and_then(Value::as_str) == Some(TAILSCALE_ENDPOINT_TAG))
    {
        bail!("endpoint tag '{TAILSCALE_ENDPOINT_TAG}' is already in use");
    }
    let mut endpoint = json!({
        "type": "tailscale",
        "tag": TAILSCALE_ENDPOINT_TAG,
        "state_directory": TAILSCALE_STATE_DIRECTORY,
        "accept_routes": true,
        "system_interface": false,
    });
    if let Some(hostname) = options.hostname.filter(|value| !value.trim().is_empty()) {
        endpoint["hostname"] = Value::String(hostname.trim().to_string());
    }
    endpoints.push(endpoint);

    root.entry("dns")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing config dns must be an object")?;
    ensure_tailscale_remote_dns_server(root)?;
    let dns = root
        .get_mut("dns")
        .and_then(Value::as_object_mut)
        .expect("DNS object was initialized");
    let servers = dns
        .entry("servers")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config dns.servers must be an array")?;
    servers.insert(
        0,
        json!({
            "type": "tailscale",
            "tag": TAILSCALE_DNS_TAG,
            "endpoint": TAILSCALE_ENDPOINT_TAG,
            "accept_default_resolvers": false,
        }),
    );
    let rules = dns
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config dns.rules must be an array")?;
    rules.splice(
        0..0,
        [
            json!({
                "domain_suffix": [tailnet_domain],
                "server": TAILSCALE_DNS_TAG,
            }),
            json!({
                "domain_suffix": ["tailscale.com", "tailscale.io"],
                "server": DEFAULT_REMOTE_DNS_TAG,
            }),
        ],
    );

    let route = root
        .entry("route")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing config route must be an object")?;
    let rules = route
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config route.rules must be an array")?;
    let insertion = rules
        .iter()
        .position(|rule| rule.get("action").and_then(Value::as_str) != Some("hijack-dns"))
        .unwrap_or(rules.len());
    rules.splice(insertion..insertion, [
        json!({"domain_suffix": [tailnet_domain], "action": "route", "outbound": TAILSCALE_ENDPOINT_TAG}),
        json!({"ip_cidr": [CGNAT_OVERLAY_CIDR, TAILSCALE_IPV6_CIDR], "action": "route", "outbound": TAILSCALE_ENDPOINT_TAG}),
        json!({"preferred_by": [TAILSCALE_ENDPOINT_TAG], "action": "route", "outbound": TAILSCALE_ENDPOINT_TAG}),
    ]);
    Ok(())
}

fn remove_managed_tailscale_config(root: &mut Map<String, Value>) -> Result<()> {
    let has_managed_endpoint = root
        .get("endpoints")
        .and_then(Value::as_array)
        .is_some_and(|endpoints| endpoints.iter().any(is_managed_tailscale_endpoint));
    let has_managed_dns_server = root
        .get("dns")
        .and_then(|dns| dns.get("servers"))
        .and_then(Value::as_array)
        .is_some_and(|servers| servers.iter().any(is_managed_tailscale_dns_server));
    if let Some(endpoints) = root.get_mut("endpoints") {
        let endpoints = endpoints
            .as_array_mut()
            .context("existing config endpoints must be an array")?;
        endpoints.retain(|endpoint| !is_managed_tailscale_endpoint(endpoint));
    }
    if let Some(dns) = root.get_mut("dns").and_then(Value::as_object_mut) {
        if let Some(servers) = dns.get_mut("servers") {
            servers
                .as_array_mut()
                .context("existing config dns.servers must be an array")?
                .retain(|server| !is_managed_tailscale_dns_server(server));
        }
        if has_managed_dns_server && let Some(rules) = dns.get_mut("rules") {
            rules
                .as_array_mut()
                .context("existing config dns.rules must be an array")?
                .retain(|rule| {
                    !is_managed_tailscale_dns_rule(rule)
                        && !is_managed_tailscale_control_plane_dns_rule(rule)
                });
        }
    }
    if has_managed_endpoint
        && let Some(route) = root.get_mut("route").and_then(Value::as_object_mut)
        && let Some(rules) = route.get_mut("rules")
    {
        rules
            .as_array_mut()
            .context("existing config route.rules must be an array")?
            .retain(|rule| !is_managed_tailscale_route_rule(rule));
    }
    Ok(())
}

fn is_managed_tailscale_dns_rule(rule: &Value) -> bool {
    let Some(rule) = rule.as_object() else {
        return false;
    };
    rule.len() == 2
        && rule.get("server").and_then(Value::as_str) == Some(TAILSCALE_DNS_TAG)
        && rule
            .get("domain_suffix")
            .and_then(Value::as_array)
            .is_some_and(|domains| domains.len() == 1 && domains[0].is_string())
}

fn is_managed_tailscale_dns_server(server: &Value) -> bool {
    server.get("type").and_then(Value::as_str) == Some("tailscale")
        && server.get("tag").and_then(Value::as_str) == Some(TAILSCALE_DNS_TAG)
        && server.get("endpoint").and_then(Value::as_str) == Some(TAILSCALE_ENDPOINT_TAG)
}

fn is_managed_tailscale_endpoint(endpoint: &Value) -> bool {
    endpoint.get("type").and_then(Value::as_str) == Some("tailscale")
        && endpoint.get("tag").and_then(Value::as_str) == Some(TAILSCALE_ENDPOINT_TAG)
        && endpoint.get("state_directory").and_then(Value::as_str)
            == Some(TAILSCALE_STATE_DIRECTORY)
        && endpoint.get("accept_routes").and_then(Value::as_bool) == Some(true)
        && endpoint.get("system_interface").and_then(Value::as_bool) == Some(false)
}

fn reject_tailscale_dns_tag_collision(root: &Map<String, Value>) -> Result<()> {
    let collision = root
        .get("dns")
        .and_then(|dns| dns.get("servers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|server| {
            server.get("tag").and_then(Value::as_str) == Some(TAILSCALE_DNS_TAG)
                && !is_managed_tailscale_dns_server(server)
        });
    if collision.is_some() {
        bail!("DNS server tag '{TAILSCALE_DNS_TAG}' is already in use");
    }
    Ok(())
}

fn ensure_tailscale_remote_dns_server(root: &mut Map<String, Value>) -> Result<()> {
    let servers = root
        .get_mut("dns")
        .and_then(Value::as_object_mut)
        .expect("DNS object was initialized")
        .entry("servers")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config dns.servers must be an array")?;
    if servers
        .iter()
        .any(|server| server.get("tag").and_then(Value::as_str) == Some(DEFAULT_REMOTE_DNS_TAG))
    {
        return Ok(());
    }
    let server = json!({
        "type": "tls",
        "tag": DEFAULT_REMOTE_DNS_TAG,
        "server": DEFAULT_REMOTE_DNS_SERVER,
        "server_port": DEFAULT_REMOTE_DNS_PORT,
    });
    servers.push(server);
    Ok(())
}

fn is_managed_tailscale_control_plane_dns_rule(rule: &Value) -> bool {
    let Some(rule) = rule.as_object() else {
        return false;
    };
    rule.len() == 2
        && rule.get("server").and_then(Value::as_str) == Some(DEFAULT_REMOTE_DNS_TAG)
        && string_array_equals(
            rule.get("domain_suffix"),
            &["tailscale.com", "tailscale.io"],
        )
}

fn is_managed_tailscale_route_rule(rule: &Value) -> bool {
    let Some(rule) = rule.as_object() else {
        return false;
    };
    if rule.len() != 3
        || rule.get("action").and_then(Value::as_str) != Some("route")
        || rule.get("outbound").and_then(Value::as_str) != Some(TAILSCALE_ENDPOINT_TAG)
    {
        return false;
    }
    rule.get("domain_suffix")
        .and_then(Value::as_array)
        .is_some_and(|values| values.len() == 1 && values[0].is_string())
        || string_array_equals(
            rule.get("ip_cidr"),
            &[CGNAT_OVERLAY_CIDR, TAILSCALE_IPV6_CIDR],
        )
        || string_array_equals(rule.get("preferred_by"), &[TAILSCALE_ENDPOINT_TAG])
}

fn string_array_equals(value: Option<&Value>, expected: &[&str]) -> bool {
    value.and_then(Value::as_array).is_some_and(|values| {
        values.len() == expected.len()
            && values
                .iter()
                .zip(expected)
                .all(|(value, expected)| value.as_str() == Some(*expected))
    })
}

fn existing_direct_outbound_tag(outbounds: &[Value]) -> Option<String> {
    outbounds
        .iter()
        .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
        .find(|tag| DIRECT_TAG_ALIASES.iter().any(|alias| alias == tag))
        .map(ToString::to_string)
        .or_else(|| {
            outbounds
                .iter()
                .find(|outbound| outbound.get("type").and_then(Value::as_str) == Some("direct"))
                .and_then(|outbound| outbound.get("tag").and_then(Value::as_str))
                .map(ToString::to_string)
        })
}

fn find_remote_dns_and_migrate_if_managed(servers: &mut [Value]) -> bool {
    let Some(remote) = servers
        .iter_mut()
        .find(|server| server.get("tag").and_then(Value::as_str) == Some(DEFAULT_REMOTE_DNS_TAG))
    else {
        return false;
    };
    let server = remote.get("server").and_then(Value::as_str);
    let is_managed_remote = remote.get("type").and_then(Value::as_str) == Some("tls")
        && matches!(
            server,
            Some(DEFAULT_REMOTE_DNS_SERVER | LEGACY_REMOTE_DNS_SERVER)
        )
        && remote.get("server_port").and_then(Value::as_u64) == Some(DEFAULT_REMOTE_DNS_PORT);
    if is_managed_remote {
        let remote = remote
            .as_object_mut()
            .expect("tagged DNS server is an object");
        remote.insert(
            "server".to_string(),
            Value::String(DEFAULT_REMOTE_DNS_SERVER.to_string()),
        );
        remote.remove("detour");
    }
    true
}

fn migrate_existing_managed_remote_dns(root: &mut serde_json::Map<String, Value>) {
    let Some(servers) = root
        .get_mut("dns")
        .and_then(Value::as_object_mut)
        .and_then(|dns| dns.get_mut("servers"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    find_remote_dns_and_migrate_if_managed(servers);
}

/// Ensures the `local` and `remote` DNS servers the China DNS rules reference actually exist,
/// adding the canonical definitions when they are missing. Remote DNS dials directly, so any
/// inherited detour is removed.
fn ensure_china_ip_routing_dns_servers(root: &mut serde_json::Map<String, Value>) -> Result<()> {
    let dns = root
        .entry("dns")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing config dns must be an object")?;
    let servers = dns
        .entry("servers")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config dns.servers must be an array")?;
    if !servers
        .iter()
        .any(|server| server.get("tag").and_then(Value::as_str) == Some(DEFAULT_LOCAL_DNS_TAG))
    {
        servers.push(json!({
            "type": "tls",
            "tag": DEFAULT_LOCAL_DNS_TAG,
            "server": "223.5.5.5",
            "server_port": 853,
        }));
    }
    if !find_remote_dns_and_migrate_if_managed(servers) {
        servers.push(json!({
            "type": "tls",
            "tag": DEFAULT_REMOTE_DNS_TAG,
            "server": DEFAULT_REMOTE_DNS_SERVER,
            "server_port": DEFAULT_REMOTE_DNS_PORT,
        }));
    }
    Ok(())
}

const RULE_MATCHER_FIELDS: &[&str] = &[
    "rule_set",
    "rules",
    "domain",
    "domain_suffix",
    "domain_keyword",
    "domain_regex",
    "ip_cidr",
    "ip_is_private",
    "source_ip_cidr",
    "source_ip_is_private",
    "port",
    "source_port",
    "network",
    "protocol",
    "process_name",
    "process_path",
    "clash_mode",
    "inbound",
    "geosite",
    "geoip",
    "query_type",
];

fn object_has_any_matcher(object: &serde_json::Map<String, Value>) -> bool {
    RULE_MATCHER_FIELDS
        .iter()
        .any(|field| match object.get(*field) {
            Some(Value::Array(items)) => !items.is_empty(),
            Some(_) => true,
            None => false,
        })
}

fn strip_rule_set_tags(rule: &mut Value, tags: &[&str]) {
    let Some(object) = rule.as_object_mut() else {
        return;
    };
    let Some(rule_set) = object.get_mut("rule_set") else {
        return;
    };
    match rule_set {
        Value::String(tag) if tags.contains(&tag.as_str()) => {
            object.remove("rule_set");
        }
        Value::Array(items) => {
            items.retain(|item| !item.as_str().is_some_and(|tag| tags.contains(&tag)));
            if items.is_empty() {
                object.remove("rule_set");
            }
        }
        _ => {}
    }
}

/// Removes the China rule-set tags from a route rule while keeping unrelated matchers. Returns
/// true when the rule is left with no matcher and must be dropped (a matcher-less rule would
/// match everything).
fn strip_china_ip_routing_from_route_rule(rule: &mut Value) -> bool {
    strip_rule_set_tags(rule, china_ip_routing_route_rule_tags());
    rule.as_object()
        .is_none_or(|object| !object_has_any_matcher(object))
}

/// Removes the China rule-set tags from a DNS rule, including nested logical rules. Returns true
/// when the rule must be dropped.
fn strip_china_ip_routing_from_dns_rule(rule: &mut Value) -> bool {
    strip_rule_set_tags(rule, china_ip_routing_rule_set_tags());
    if let Some(object) = rule.as_object_mut()
        && let Some(Value::Array(nested)) = object.get_mut("rules")
    {
        nested.retain(|nested| {
            !nested
                .get("rule_set")
                .and_then(Value::as_str)
                .is_some_and(|tag| china_ip_routing_rule_set_tags().contains(&tag))
        });
        if nested.is_empty() {
            object.remove("rules");
        }
    }
    rule.as_object()
        .is_none_or(|object| !object_has_any_matcher(object))
}

fn ensure_china_ip_routing(config: &mut Value, enable: bool, ruleset_dir: &Path) -> Result<()> {
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;

    // Resolve the direct outbound up front so a missing one is rejected before any mutation.
    let direct_tag = if enable {
        root.get("outbounds")
            .and_then(Value::as_array)
            .and_then(|outbounds| existing_direct_outbound_tag(outbounds))
            .context("cannot enable China IP routing: no direct outbound in the config")?
    } else {
        String::new()
    };

    {
        let route = root
            .entry("route")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("existing config route must be an object")?;

        let rule_sets = route
            .entry("rule_set")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .context("existing config route.rule_set must be an array")?;
        rule_sets.retain(|rule_set| {
            !china_ip_routing_rule_set_tags().contains(
                &rule_set
                    .get("tag")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        });
        if enable {
            rule_sets.extend(china_ip_routing_local_rule_sets(ruleset_dir));
        }

        let rules = route
            .entry("rules")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .context("existing config route.rules must be an array")?;
        rules.retain_mut(|rule| !strip_china_ip_routing_from_route_rule(rule));
        if enable {
            let route_rules = china_ip_routing_route_rules(&direct_tag);
            let index = clash_mode_rule_insert_index(rules);
            for (offset, rule) in route_rules.into_iter().enumerate() {
                rules.insert(index + offset, rule);
            }
        }
    }

    if enable {
        ensure_china_ip_routing_dns_servers(root)?;
    }
    let dns = root
        .entry("dns")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing config dns must be an object")?;
    let dns_rule_values = dns
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config dns.rules must be an array")?;
    dns_rule_values.retain_mut(|rule| !strip_china_ip_routing_from_dns_rule(rule));
    if enable {
        let dns_rules = china_ip_routing_dns_rules();
        let index = clash_mode_rule_insert_index(dns_rule_values);
        for (offset, rule) in dns_rules.into_iter().enumerate() {
            dns_rule_values.insert(index + offset, rule);
        }
    }

    Ok(())
}

#[cfg(test)]
pub(crate) fn ensure_hillstone_route_table(
    config: &mut Value,
    options: HillstoneRouteTableOptions,
) -> Result<()> {
    ensure_private_access_route_table(
        config,
        PrivateAccessRouteTableOptions {
            profile_id: "hillstone".to_string(),
            cidrs: options.cidrs,
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            previous_cidrs: Vec::new(),
            previous_domains: Vec::new(),
            previous_domain_suffixes: Vec::new(),
            carrier_domains: Vec::new(),
            proxy: Some(options.proxy),
        },
    )
}

pub(crate) fn ensure_private_access_route_table(
    config: &mut Value,
    options: PrivateAccessRouteTableOptions,
) -> Result<()> {
    if options.profile_id.trim().is_empty() {
        anyhow::bail!("private access profile id cannot be empty");
    }
    if options.proxy.is_none() {
        ensure_private_access_tun_baseline(config, &options.carrier_domains)?;
    } else {
        ensure_private_access_carrier_route(config, &options.carrier_domains)?;
    }
    let target_cidrs = normalize_ipv4_cidrs(&options.cidrs)?;
    let target_domains = normalize_private_access_domains(&options.domains);
    let target_domain_suffixes = normalize_private_access_domains(&options.domain_suffixes);
    let previous_cidrs = normalize_ipv4_cidrs(&options.previous_cidrs)?;
    let previous_domains = normalize_private_access_domains(&options.previous_domains);
    let previous_domain_suffixes =
        normalize_private_access_domains(&options.previous_domain_suffixes);
    if target_cidrs.is_empty() && target_domains.is_empty() && target_domain_suffixes.is_empty() {
        return Ok(());
    }

    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let outbounds_value = root
        .entry("outbounds")
        .or_insert_with(|| Value::Array(Vec::new()));
    let outbounds = outbounds_value
        .as_array_mut()
        .context("existing config outbounds must be an array")?;
    let direct_tag = preferred_existing_tag(outbounds, DIRECT_TAG_ALIASES, DEFAULT_DIRECT_TAG);
    upsert_special_outbound(
        outbounds,
        &direct_tag,
        || json!({ "type": "direct", "tag": direct_tag }),
        |value| {
            ensure_string_field(value, "type", "direct");
        },
    )?;

    let route_value = root.entry("route").or_insert_with(|| json!({}));
    let route = route_value
        .as_object_mut()
        .context("existing config route must be an object")?;
    let rules_value = route
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()));
    let rules = rules_value
        .as_array_mut()
        .context("existing config route.rules must be an array")?;
    let managed_targets = PrivateAccessManagedTargets {
        current: PrivateAccessRouteTargets {
            cidrs: &target_cidrs,
            domains: &target_domains,
            domain_suffixes: &target_domain_suffixes,
        },
        previous: PrivateAccessRouteTargets {
            cidrs: &previous_cidrs,
            domains: &previous_domains,
            domain_suffixes: &previous_domain_suffixes,
        },
        direct_tag: &direct_tag,
        proxy: options.proxy,
    };
    rules.retain(|rule| !rule_matches_private_access_route_targets(rule, &managed_targets));

    // Profile-owned route metadata cannot be written into sing-box route rules because sing-box
    // may reject unknown fields. Ownership is tracked by the TUI/Private Access session; the config rule
    // itself stays valid sing-box JSON and intentionally has no port matcher.
    let mut index = hillstone_route_insert_index(rules);
    if !target_domains.is_empty() || !target_domain_suffixes.is_empty() {
        let mut resolve_rule = json!({
            "action": "resolve",
            "server": PRIVATE_ACCESS_SYSTEM_DNS_TAG,
            "strategy": "ipv4_only",
            "disable_cache": true,
        });
        set_private_access_domain_matchers(
            &mut resolve_rule,
            &target_domains,
            &target_domain_suffixes,
        );
        rules.insert(index, resolve_rule);
        index += 1;

        let mut domain_rule = json!({
            "action": "route",
            "outbound": direct_tag,
        });
        set_private_access_domain_matchers(
            &mut domain_rule,
            &target_domains,
            &target_domain_suffixes,
        );
        if let Some(proxy) = options.proxy {
            domain_rule["override_address"] = Value::String(proxy.ip().to_string());
            domain_rule["override_port"] = Value::from(proxy.port());
        }
        rules.insert(index, domain_rule);
        index += 1;
    }
    if !target_cidrs.is_empty() {
        let mut cidr_rule = json!({
            "action": "route",
            "ip_cidr": target_cidrs.iter().map(Ipv4Cidr::to_string).collect::<Vec<_>>(),
            "outbound": direct_tag,
        });
        if let Some(proxy) = options.proxy {
            cidr_rule["override_address"] = Value::String(proxy.ip().to_string());
            cidr_rule["override_port"] = Value::from(proxy.port());
        }
        rules.insert(index, cidr_rule);
    }
    Ok(())
}

pub(crate) fn build_full_config_with_provider_groups_and_options(
    config_path: &PathBuf,
    imported_nodes: Vec<Value>,
    replace_nodes: bool,
    provider_name: &str,
    existing_provider_name: Option<&str>,
    default_config_options: DefaultConfigOptions,
) -> Result<Value> {
    let imported_node_tags = collect_tags(&imported_nodes);
    let mut existing_node_tags = Vec::new();

    let mut config = if config_path.exists() {
        let text = fs::read_to_string(config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut config: Value = parse_sing_box_config_text(&text)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        if let Some(outbounds) = config.get("outbounds").and_then(Value::as_array)
            && !replace_nodes
        {
            existing_node_tags = outbounds
                .iter()
                .filter(|outbound| is_replaceable_node_outbound(outbound))
                .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
                .filter(|tag| !is_metadata_node_tag(tag))
                .map(ToString::to_string)
                .collect();
        }
        merge_into_existing_config(&mut config, imported_nodes, replace_nodes)?;
        config
    } else {
        build_default_config_with_options(imported_nodes, default_config_options)
    };

    add_provider_groups(
        &mut config,
        provider_name,
        &imported_node_tags,
        existing_provider_name,
        &existing_node_tags,
    )?;
    Ok(config)
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderNodeSet {
    pub(crate) provider_name: String,
    pub(crate) nodes: Vec<Value>,
}

#[cfg(test)]
pub(crate) fn build_full_config_with_provider_node_sets(
    config_path: &PathBuf,
    provider_node_sets: Vec<ProviderNodeSet>,
    replace_nodes: bool,
) -> Result<Value> {
    build_full_config_with_provider_node_sets_and_options(
        config_path,
        provider_node_sets,
        replace_nodes,
        DefaultConfigOptions::default(),
    )
}

pub(crate) fn build_full_config_with_provider_node_sets_and_options(
    config_path: &PathBuf,
    provider_node_sets: Vec<ProviderNodeSet>,
    replace_nodes: bool,
    default_config_options: DefaultConfigOptions,
) -> Result<Value> {
    let provider_node_tags = provider_node_sets
        .iter()
        .map(|set| (set.provider_name.clone(), collect_tags(&set.nodes)))
        .collect::<Vec<_>>();
    let imported_nodes = provider_node_sets
        .into_iter()
        .flat_map(|set| set.nodes)
        .collect::<Vec<_>>();

    let mut config = if config_path.exists() {
        let text = fs::read_to_string(config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut config: Value = parse_sing_box_config_text(&text)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        remove_provider_managed_nodes(&mut config, &provider_node_tags)?;
        merge_into_existing_config(&mut config, imported_nodes, replace_nodes)?;
        config
    } else {
        build_default_config_with_options(imported_nodes, default_config_options)
    };

    add_multiple_provider_groups(&mut config, &provider_node_tags)?;
    Ok(config)
}

#[cfg(test)]
pub(crate) fn build_default_config(imported_nodes: Vec<Value>) -> Value {
    build_default_config_with_options(imported_nodes, DefaultConfigOptions::default())
}

pub(crate) fn build_default_config_with_options(
    imported_nodes: Vec<Value>,
    options: DefaultConfigOptions,
) -> Value {
    let node_tags = collect_tags(&imported_nodes);
    let select_members = with_leading_members(&[DEFAULT_AUTO_SELECTOR_TAG], &node_tags);
    let mut inbounds = vec![json!({
        "type": "mixed",
        "listen": "::",
        "listen_port": 6780,
        "set_system_proxy": false,
    })];
    if options.include_tun_mode {
        inbounds.push(default_tun_inbound());
    }

    let mut outbounds = Vec::with_capacity(imported_nodes.len() + 5);
    outbounds.push(json!({
        "type": "selector",
        "tag": DEFAULT_SELECTOR_TAG,
        "outbounds": select_members,
        "default": DEFAULT_AUTO_SELECTOR_TAG,
        "interrupt_exist_connections": false,
    }));
    outbounds.push(json!({
        "type": "urltest",
        "tag": DEFAULT_AUTO_SELECTOR_TAG,
        "outbounds": node_tags,
        "interrupt_exist_connections": false,
    }));
    outbounds.push(json!({
        "type": "selector",
        "tag": DEFAULT_AD_BLOCK_SELECTOR_TAG,
        "outbounds": [DEFAULT_SELECTOR_TAG, DEFAULT_DIRECT_TAG, DEFAULT_BLOCK_TAG],
        "default": DEFAULT_BLOCK_TAG,
        "interrupt_exist_connections": false,
    }));
    outbounds.extend(imported_nodes);
    outbounds.push(json!({
        "type": "direct",
        "tag": DEFAULT_DIRECT_TAG,
    }));
    outbounds.push(json!({
        "type": "block",
        "tag": DEFAULT_BLOCK_TAG,
    }));

    let mut dns_rules = vec![
        json!({
            "clash_mode": "全局",
            "server": DEFAULT_REMOTE_DNS_TAG,
        }),
        json!({
            "clash_mode": "直连",
            "server": DEFAULT_LOCAL_DNS_TAG,
        }),
    ];
    if options.include_geosite_rules {
        dns_rules.extend(china_ip_routing_dns_rules());
    }
    dns_rules.push(json!({
        "clash_mode": "规则",
        "server": DEFAULT_REMOTE_DNS_TAG,
    }));

    let mut route_rules = vec![
        json!({
            "type": "logical",
            "mode": "or",
            "rules": [
                {
                    "protocol": "dns",
                },
                {
                    "port": 53,
                }
            ],
            "action": "hijack-dns",
        }),
        json!({
            "domain_suffix": [
                "airtcp.me",
                "airtcp.com",
                "airapp.link",
                "mailrelay.us"
            ],
            "outbound": DEFAULT_DIRECT_TAG,
        }),
        json!({
            "rule_set": DEFAULT_BYPASS_RULE_SET_TAG,
            "outbound": DEFAULT_DIRECT_TAG,
        }),
        json!({
            "ip_cidr": [CGNAT_OVERLAY_CIDR],
            "outbound": DEFAULT_DIRECT_TAG,
        }),
        json!({
            "clash_mode": "直连",
            "outbound": DEFAULT_DIRECT_TAG,
        }),
        json!({
            "clash_mode": "全局",
            "outbound": DEFAULT_SELECTOR_TAG,
        }),
        json!({
            "ip_is_private": true,
            "outbound": DEFAULT_DIRECT_TAG,
        }),
    ];
    if options.include_geosite_rules {
        route_rules.extend(china_ip_routing_route_rules(DEFAULT_DIRECT_TAG));
        route_rules.push(json!({
            "rule_set": "AdGuardSDNSFilter",
            "outbound": DEFAULT_AD_BLOCK_SELECTOR_TAG,
        }));
    }
    route_rules.push(json!({
        "clash_mode": "规则",
        "outbound": DEFAULT_SELECTOR_TAG,
    }));

    let mut route_rule_sets = vec![json!({
        "type": "local",
        "tag": DEFAULT_BYPASS_RULE_SET_TAG,
        "format": "source",
        "path": DEFAULT_BYPASS_RULE_SET_PATH,
    })];
    if options.include_geosite_rules {
        route_rule_sets.extend(china_ip_routing_rule_sets(DEFAULT_DIRECT_TAG));
        route_rule_sets.push(json!({
            "type": "remote",
            "tag": "AdGuardSDNSFilter",
            "format": "binary",
            "url": "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/category-ads-all.srs",
            "download_detour": DEFAULT_DIRECT_TAG,
            "update_interval": "30d",
        }));
    }

    json!({
        "log": {
            "level": "error",
            "output": "core.log",
            "timestamp": true,
        },
        "dns": {
            "servers": [
                {
                    "type": "tls",
                    "tag": DEFAULT_REMOTE_DNS_TAG,
                    "server": DEFAULT_REMOTE_DNS_SERVER,
                    "server_port": DEFAULT_REMOTE_DNS_PORT,
                },
                {
                    "type": "tls",
                    "tag": DEFAULT_LOCAL_DNS_TAG,
                    "server": "223.5.5.5",
                    "server_port": 853,
                }
            ],
            "rules": dns_rules,
            "strategy": "ipv4_only",
            "independent_cache": false,
        },
        "inbounds": inbounds,
        "outbounds": outbounds,
        "route": {
            "default_domain_resolver": {
                "server": DEFAULT_LOCAL_DNS_TAG,
                "strategy": "ipv4_only",
            },
            "rules": route_rules,
            "rule_set": route_rule_sets,
            "auto_detect_interface": true,
        },
        "experimental": {
            "cache_file": {
                "enabled": true,
                "store_rdrc": true,
            },
            "clash_api": {
                "external_controller": "0.0.0.0:9992",
                "default_mode": "规则",
            }
        }
    })
}

pub(crate) fn merge_into_existing_config(
    config: &mut Value,
    imported_nodes: Vec<Value>,
    replace_nodes: bool,
) -> Result<()> {
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    migrate_legacy_inbound_fields(root)?;
    migrate_existing_managed_remote_dns(root);
    let outbounds_value = root
        .entry("outbounds")
        .or_insert_with(|| Value::Array(Vec::new()));
    let outbounds = outbounds_value
        .as_array_mut()
        .context("existing config outbounds must be an array")?;

    let selector_tag =
        preferred_existing_tag(outbounds, SELECTOR_TAG_ALIASES, DEFAULT_SELECTOR_TAG);
    let prefers_legacy_tags = selector_tag == "select";
    let auto_tag = preferred_existing_tag(
        outbounds,
        AUTO_SELECTOR_TAG_ALIASES,
        if prefers_legacy_tags {
            "auto"
        } else {
            DEFAULT_AUTO_SELECTOR_TAG
        },
    );
    let direct_tag = preferred_existing_tag(
        outbounds,
        DIRECT_TAG_ALIASES,
        if prefers_legacy_tags {
            "direct"
        } else {
            DEFAULT_DIRECT_TAG
        },
    );
    let block_tag = preferred_existing_tag(
        outbounds,
        BLOCK_TAG_ALIASES,
        if prefers_legacy_tags {
            "block"
        } else {
            DEFAULT_BLOCK_TAG
        },
    );

    upsert_special_outbound(
        outbounds,
        &direct_tag,
        || json!({ "type": "direct", "tag": direct_tag }),
        |_| {},
    )?;
    upsert_special_outbound(
        outbounds,
        &block_tag,
        || json!({ "type": "block", "tag": block_tag }),
        |_| {},
    )?;

    if replace_nodes {
        outbounds.retain(|outbound| !is_replaceable_node_outbound(outbound));
    }

    let node_tags = collect_tags(&imported_nodes);
    let select_members = with_leading_members(&[&auto_tag], &node_tags);

    upsert_special_outbound(
        outbounds,
        &selector_tag,
        || {
            json!({
                "type": "selector",
                "tag": selector_tag,
                "outbounds": select_members,
                "default": auto_tag,
            })
        },
        |value| {
            if replace_nodes {
                set_outbound_members(value, &select_members);
            } else {
                merge_outbound_members(value, &select_members);
            }
            ensure_string_field(value, "type", "selector");
            ensure_string_field(value, "default", &auto_tag);
            set_bool_field(value, "interrupt_exist_connections", false);
        },
    )?;

    upsert_special_outbound(
        outbounds,
        &auto_tag,
        || {
            json!({
                "type": "urltest",
                "tag": auto_tag,
                "outbounds": node_tags,
                "url": DEFAULT_DELAY_TEST_URL,
                "interval": "10m",
            })
        },
        |value| {
            if replace_nodes {
                set_outbound_members(value, &node_tags);
            } else {
                merge_outbound_members(value, &node_tags);
            }
            ensure_string_field(value, "type", "urltest");
            ensure_string_field(value, "url", DEFAULT_DELAY_TEST_URL);
            ensure_string_field(value, "interval", "10m");
            set_bool_field(value, "interrupt_exist_connections", false);
        },
    )?;

    for node in imported_nodes {
        upsert_tagged_outbound(outbounds, node)?;
    }

    let route_value = root.entry("route").or_insert_with(|| json!({}));
    let route = route_value
        .as_object_mut()
        .context("existing config route must be an object")?;
    route
        .entry("final")
        .or_insert_with(|| Value::String(selector_tag));
    ensure_subscription_direct_route(route, &direct_tag)?;
    ensure_bypass_route(route, &direct_tag)?;
    ensure_cgnat_direct_route(route, &direct_tag)?;

    let experimental_value = root.entry("experimental").or_insert_with(|| json!({}));
    let experimental = experimental_value
        .as_object_mut()
        .context("existing config experimental must be an object")?;
    let cache_file_value = experimental
        .entry("cache_file")
        .or_insert_with(|| json!({ "enabled": true }));
    let cache_file = cache_file_value
        .as_object_mut()
        .context("existing config experimental.cache_file must be an object")?;
    cache_file
        .entry("enabled")
        .or_insert_with(|| Value::Bool(true));

    let clash_api_value = experimental.entry("clash_api").or_insert_with(|| {
        json!({
            "external_controller": default_clash_api_external_controller(),
            "secret": "",
        })
    });
    let clash_api = clash_api_value
        .as_object_mut()
        .context("existing config experimental.clash_api must be an object")?;
    clash_api
        .entry("external_controller")
        .or_insert_with(|| Value::String(default_clash_api_external_controller().to_string()));
    clash_api
        .entry("secret")
        .or_insert_with(|| Value::String(String::new()));

    Ok(())
}

fn add_provider_groups(
    config: &mut Value,
    provider_name: &str,
    provider_node_tags: &[String],
    existing_provider_name: Option<&str>,
    existing_node_tags: &[String],
) -> Result<()> {
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let outbounds = root
        .get_mut("outbounds")
        .and_then(Value::as_array_mut)
        .context("existing config outbounds must be an array")?;
    outbounds.retain(|outbound| {
        !is_replaceable_node_outbound(outbound)
            || outbound
                .get("tag")
                .and_then(Value::as_str)
                .is_some_and(|tag| !is_metadata_node_tag(tag))
    });

    let selector_tag =
        preferred_existing_tag(outbounds, SELECTOR_TAG_ALIASES, DEFAULT_SELECTOR_TAG);
    let prefers_legacy_tags = selector_tag == "select";
    let auto_tag = preferred_existing_tag(
        outbounds,
        AUTO_SELECTOR_TAG_ALIASES,
        if prefers_legacy_tags {
            "auto"
        } else {
            DEFAULT_AUTO_SELECTOR_TAG
        },
    );
    let mut provider_tags = Vec::new();
    if let Some(existing_provider_name) = existing_provider_name
        && !existing_node_tags.is_empty()
    {
        upsert_provider_selector(outbounds, existing_provider_name, existing_node_tags)?;
        provider_tags.push(existing_provider_name.to_string());
    }
    if !provider_node_tags.is_empty() {
        upsert_provider_selector(outbounds, provider_name, provider_node_tags)?;
        provider_tags.push(provider_name.to_string());
    }
    if provider_tags.len() < 2 {
        return Ok(());
    }

    let select_members = with_leading_members(&[auto_tag.as_str()], &provider_tags);
    if let Some(selector) = find_outbound_by_tag_mut(outbounds, &selector_tag) {
        set_outbound_members(selector, &select_members);
        ensure_string_field(selector, "type", "selector");
        ensure_string_field(selector, "default", &auto_tag);
    }

    let all_node_tags = collect_tags(
        &outbounds
            .iter()
            .filter(|outbound| is_replaceable_node_outbound(outbound))
            .filter(|outbound| {
                outbound
                    .get("tag")
                    .and_then(Value::as_str)
                    .is_some_and(|tag| !is_metadata_node_tag(tag))
            })
            .cloned()
            .collect::<Vec<_>>(),
    );
    if let Some(auto) = find_outbound_by_tag_mut(outbounds, &auto_tag) {
        set_outbound_members(auto, &all_node_tags);
        ensure_string_field(auto, "type", "urltest");
        ensure_string_field(auto, "url", DEFAULT_DELAY_TEST_URL);
        ensure_string_field(auto, "interval", "10m");
    }

    Ok(())
}

fn add_multiple_provider_groups(
    config: &mut Value,
    provider_node_tags: &[(String, Vec<String>)],
) -> Result<()> {
    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let outbounds = root
        .get_mut("outbounds")
        .and_then(Value::as_array_mut)
        .context("existing config outbounds must be an array")?;
    outbounds.retain(|outbound| {
        !is_replaceable_node_outbound(outbound)
            || outbound
                .get("tag")
                .and_then(Value::as_str)
                .is_some_and(|tag| !is_metadata_node_tag(tag))
    });

    let mut provider_tags = Vec::new();
    let mut grouped_node_tags = BTreeSet::new();
    for (provider_name, node_tags) in provider_node_tags {
        if node_tags.is_empty() {
            continue;
        }
        upsert_provider_selector(outbounds, provider_name, node_tags)?;
        provider_tags.push(provider_name.clone());
        grouped_node_tags.extend(node_tags.iter().cloned());
    }
    if provider_node_tags.is_empty() {
        return Ok(());
    }

    let selector_tag =
        preferred_existing_tag(outbounds, SELECTOR_TAG_ALIASES, DEFAULT_SELECTOR_TAG);
    let prefers_legacy_tags = selector_tag == "select";
    let auto_tag = preferred_existing_tag(
        outbounds,
        AUTO_SELECTOR_TAG_ALIASES,
        if prefers_legacy_tags {
            "auto"
        } else {
            DEFAULT_AUTO_SELECTOR_TAG
        },
    );
    let all_node_tags = collect_tags(
        &outbounds
            .iter()
            .filter(|outbound| is_replaceable_node_outbound(outbound))
            .filter(|outbound| {
                outbound
                    .get("tag")
                    .and_then(Value::as_str)
                    .is_some_and(|tag| !is_metadata_node_tag(tag))
            })
            .cloned()
            .collect::<Vec<_>>(),
    );
    let ungrouped_node_tags = all_node_tags
        .iter()
        .filter(|tag| !grouped_node_tags.contains(*tag))
        .cloned()
        .collect::<Vec<_>>();

    let mut select_members = with_leading_members(&[auto_tag.as_str()], &provider_tags);
    for tag in ungrouped_node_tags {
        if !select_members.contains(&tag) {
            select_members.push(tag);
        }
    }
    if let Some(selector) = find_outbound_by_tag_mut(outbounds, &selector_tag) {
        set_outbound_members(selector, &select_members);
        ensure_string_field(selector, "type", "selector");
        ensure_string_field(selector, "default", &auto_tag);
    }

    if let Some(auto) = find_outbound_by_tag_mut(outbounds, &auto_tag) {
        set_outbound_members(auto, &all_node_tags);
        ensure_string_field(auto, "type", "urltest");
        ensure_string_field(auto, "url", DEFAULT_DELAY_TEST_URL);
        ensure_string_field(auto, "interval", "10m");
    }

    Ok(())
}

fn upsert_provider_selector(
    outbounds: &mut Vec<Value>,
    provider_name: &str,
    node_tags: &[String],
) -> Result<()> {
    upsert_special_outbound(
        outbounds,
        provider_name,
        || {
            json!({
                "type": "selector",
                "tag": provider_name,
                "outbounds": node_tags,
                "default": node_tags.first().cloned().unwrap_or_default(),
                "interrupt_exist_connections": true,
            })
        },
        |value| {
            set_outbound_members(value, node_tags);
            ensure_string_field(value, "type", "selector");
            if let Some(first) = node_tags.first() {
                ensure_string_field(value, "default", first);
            }
        },
    )
}

fn migrate_legacy_inbound_fields(root: &mut serde_json::Map<String, Value>) -> Result<()> {
    let Some(inbounds_value) = root.get_mut("inbounds") else {
        return Ok(());
    };
    let inbounds = inbounds_value
        .as_array_mut()
        .context("existing config inbounds must be an array")?;
    if !cfg!(target_os = "linux") {
        inbounds.retain(|inbound| !is_default_auto_redirect_tun_inbound(inbound));
    }

    let mut sniff = false;
    let mut sniff_timeout = None;
    let mut strategy = None;

    for inbound in inbounds {
        let Some(object) = inbound.as_object_mut() else {
            continue;
        };

        if object
            .remove("sniff")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            sniff = true;
        }
        if let Some(value) = object.remove("sniff_timeout").and_then(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        }) {
            sniff_timeout.get_or_insert(value);
        }
        if let Some(value) = object.remove("domain_strategy").and_then(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        }) {
            strategy.get_or_insert(value);
        }
        object.remove("sniff_override_destination");
        object.remove("udp_disable_domain_unmapping");
    }

    let mut rules_to_prepend = Vec::new();
    if let Some(strategy) = strategy {
        rules_to_prepend.push(json!({
            "action": "resolve",
            "strategy": strategy,
        }));
    }
    if sniff {
        let mut rule = json!({ "action": "sniff" });
        if let Some(timeout) = sniff_timeout {
            rule.as_object_mut()
                .expect("sniff rule is an object")
                .insert("timeout".to_string(), Value::String(timeout));
        }
        rules_to_prepend.push(rule);
    }
    if rules_to_prepend.is_empty() {
        return Ok(());
    }

    let route_value = root.entry("route").or_insert_with(|| json!({}));
    let route = route_value
        .as_object_mut()
        .context("existing config route must be an object")?;
    let rules_value = route
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()));
    let rules = rules_value
        .as_array_mut()
        .context("existing config route.rules must be an array")?;

    for rule in rules_to_prepend.into_iter().rev() {
        if !rules.iter().any(|existing| existing == &rule) {
            rules.insert(0, rule);
        }
    }

    Ok(())
}

fn is_default_auto_redirect_tun_inbound(inbound: &Value) -> bool {
    inbound.as_object().is_some_and(|object| {
        object.get("type").and_then(Value::as_str) == Some("tun")
            && object
                .get("auto_redirect")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && object
                .get("auto_route")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && object
                .get("strict_route")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && object.get("stack").and_then(Value::as_str) == Some("mixed")
    })
}

fn remove_provider_managed_nodes(
    config: &mut Value,
    provider_node_tags: &[(String, Vec<String>)],
) -> Result<()> {
    let provider_names = provider_node_tags
        .iter()
        .map(|(provider_name, _)| provider_name.as_str())
        .collect::<BTreeSet<_>>();
    if provider_names.is_empty() {
        return Ok(());
    }

    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let Some(outbounds) = root.get_mut("outbounds").and_then(Value::as_array_mut) else {
        return Ok(());
    };

    let stale_node_tags = outbounds
        .iter()
        .filter(|outbound| {
            outbound
                .get("tag")
                .and_then(Value::as_str)
                .is_some_and(|tag| provider_names.contains(tag))
        })
        .filter_map(|outbound| outbound.get("outbounds").and_then(Value::as_array))
        .flat_map(|members| members.iter().filter_map(Value::as_str))
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();

    outbounds.retain(|outbound| {
        !is_managed_provider_selector(outbound, &provider_names)
            && (!is_replaceable_node_outbound(outbound)
                || outbound
                    .get("tag")
                    .and_then(Value::as_str)
                    .is_none_or(|tag| !stale_node_tags.contains(tag)))
    });
    Ok(())
}

fn is_managed_provider_selector(outbound: &Value, provider_names: &BTreeSet<&str>) -> bool {
    let outbound_type = outbound
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    outbound_type.eq_ignore_ascii_case("selector")
        && outbound
            .get("tag")
            .and_then(Value::as_str)
            .is_some_and(|tag| provider_names.contains(tag))
}

fn ensure_subscription_direct_route(
    route: &mut serde_json::Map<String, Value>,
    direct_tag: &str,
) -> Result<()> {
    let rule = json!({
        "domain_suffix": [
            "airtcp.me",
            "airtcp.com",
            "airapp.link",
            "mailrelay.us"
        ],
        "outbound": direct_tag,
    });
    let rules_value = route
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()));
    let rules = rules_value
        .as_array_mut()
        .context("existing config route.rules must be an array")?;
    rules.retain(|existing| existing != &rule);
    let index = rules
        .iter()
        .rposition(|existing| existing.get("action").is_some())
        .map_or(0, |index| index + 1);
    rules.insert(index, rule);
    Ok(())
}

fn ensure_bypass_route(route: &mut serde_json::Map<String, Value>, direct_tag: &str) -> Result<()> {
    let rules_value = route
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()));
    let rules = rules_value
        .as_array_mut()
        .context("existing config route.rules must be an array")?;
    if let Some(rule) = rules
        .iter_mut()
        .find(|rule| rule_references_rule_set(rule, DEFAULT_BYPASS_RULE_SET_TAG))
    {
        set_rule_outbound(rule, direct_tag);
    } else {
        let index = if rules.first().is_some_and(is_dns_hijack_rule) {
            1
        } else {
            0
        };
        rules.insert(
            index,
            json!({
                "rule_set": DEFAULT_BYPASS_RULE_SET_TAG,
                "outbound": direct_tag,
            }),
        );
    }

    let rule_sets_value = route
        .entry("rule_set")
        .or_insert_with(|| Value::Array(Vec::new()));
    let rule_sets = rule_sets_value
        .as_array_mut()
        .context("existing config route.rule_set must be an array")?;
    if let Some(rule_set) = rule_sets.iter_mut().find(|value| {
        value
            .get("tag")
            .and_then(Value::as_str)
            .is_some_and(|tag| tag == DEFAULT_BYPASS_RULE_SET_TAG)
    }) {
        if let Some(object) = rule_set.as_object_mut() {
            object.insert("type".to_string(), Value::String("local".to_string()));
            object.insert("format".to_string(), Value::String("source".to_string()));
            object.insert(
                "path".to_string(),
                Value::String(DEFAULT_BYPASS_RULE_SET_PATH.to_string()),
            );
        }
    } else {
        rule_sets.insert(
            0,
            json!({
                "type": "local",
                "tag": DEFAULT_BYPASS_RULE_SET_TAG,
                "format": "source",
                "path": DEFAULT_BYPASS_RULE_SET_PATH,
            }),
        );
    }
    Ok(())
}

fn ensure_cgnat_direct_route(
    route: &mut serde_json::Map<String, Value>,
    direct_tag: &str,
) -> Result<()> {
    let rules_value = route
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()));
    let rules = rules_value
        .as_array_mut()
        .context("existing config route.rules must be an array")?;
    if let Some(rule) = rules
        .iter_mut()
        .find(|rule| rule_matches_cgnat_overlay_cidr(rule))
    {
        set_rule_outbound(rule, direct_tag);
        return Ok(());
    }

    let index = rules
        .iter()
        .position(|rule| rule_references_rule_set(rule, DEFAULT_BYPASS_RULE_SET_TAG))
        .map(|index| index + 1)
        .or_else(|| {
            rules
                .iter()
                .rposition(|existing| existing.get("action").is_some())
                .map(|index| index + 1)
        })
        .unwrap_or(0);
    rules.insert(
        index,
        json!({
            "ip_cidr": [CGNAT_OVERLAY_CIDR],
            "outbound": direct_tag,
        }),
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ipv4Cidr {
    network: Ipv4Addr,
    prefix_len: u32,
}

impl Ipv4Cidr {
    fn parse(value: &str) -> Result<Self> {
        let (ip, prefix_len) = value
            .split_once('/')
            .with_context(|| format!("expected IPv4 CIDR, got {value}"))?;
        let ip = ip
            .parse::<Ipv4Addr>()
            .with_context(|| format!("invalid IPv4 CIDR address: {value}"))?;
        let prefix_len = prefix_len
            .parse::<u32>()
            .with_context(|| format!("invalid IPv4 CIDR prefix length: {value}"))?;
        if prefix_len > 32 {
            anyhow::bail!("invalid IPv4 CIDR prefix length: {value}");
        }
        let network = Ipv4Addr::from(u32::from(ip) & prefix_mask(prefix_len));
        Ok(Self {
            network,
            prefix_len,
        })
    }

    fn overlaps(self, other: Self) -> bool {
        let prefix_len = self.prefix_len.min(other.prefix_len);
        (u32::from(self.network) & prefix_mask(prefix_len))
            == (u32::from(other.network) & prefix_mask(prefix_len))
    }
}

impl std::fmt::Display for Ipv4Cidr {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.network, self.prefix_len)
    }
}

fn prefix_mask(prefix_len: u32) -> u32 {
    if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    }
}

fn normalize_ipv4_cidrs(values: &[String]) -> Result<Vec<Ipv4Cidr>> {
    let mut seen = BTreeSet::new();
    let mut cidrs = Vec::new();
    for value in values {
        let cidr = Ipv4Cidr::parse(value)?;
        if seen.insert(cidr.to_string()) {
            cidrs.push(cidr);
        }
    }
    Ok(cidrs)
}

fn ensure_private_access_tun_baseline(
    config: &mut Value,
    carrier_domains: &[String],
) -> Result<()> {
    let tun_state = tun_config_state(config)?;
    let private_access_baseline_already_owned = config_has_private_access_tun_baseline(config);
    ensure_private_access_carrier_route(config, carrier_domains)?;

    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let outbounds = root
        .entry("outbounds")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config outbounds must be an array")?;
    let direct_tag = preferred_existing_tag(outbounds, DIRECT_TAG_ALIASES, DEFAULT_DIRECT_TAG);
    upsert_special_outbound(
        outbounds,
        &direct_tag,
        || json!({ "type": "direct", "tag": direct_tag }),
        |value| {
            ensure_string_field(value, "type", "direct");
        },
    )?;
    ensure_private_access_system_dns(root)?;

    let route = root
        .entry("route")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing config route must be an object")?;
    if tun_state.managed_internet_tun {
        // Internet TUN needs physical-interface binding to prevent its own proxy/DNS dials from
        // looping back into the default TUN route.
        route.insert("auto_detect_interface".to_string(), Value::Bool(true));
    } else if !tun_state.other_tun && !private_access_baseline_already_owned {
        // Without a sing-box TUN, leave Private Access dials unbound so its more-specific helper
        // routes can win on first setup. Once the baseline marker exists, preserve explicit user
        // edits and values restored by the Internet TUN toggle. A custom TUN always owns its own
        // routing policy.
        route.insert("auto_detect_interface".to_string(), Value::Bool(false));
    }
    let rules = route
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config route.rules must be an array")?;
    if !rules.iter().any(|rule| {
        rule.get("ip_is_private").and_then(Value::as_bool) == Some(true)
            && rule.get("outbound").and_then(Value::as_str) == Some(direct_tag.as_str())
            && rule.get("override_address").is_none()
            && rule.get("override_port").is_none()
    }) {
        let index = rules
            .iter()
            .position(|rule| rule.get("clash_mode").is_some())
            .unwrap_or(rules.len());
        rules.insert(
            index,
            json!({
                "ip_is_private": true,
                "outbound": direct_tag,
            }),
        );
    }
    Ok(())
}

fn ensure_private_access_carrier_route(
    config: &mut Value,
    carrier_domains: &[String],
) -> Result<()> {
    let carrier_domains = normalize_private_access_domains(carrier_domains);
    if carrier_domains.is_empty() {
        return Ok(());
    }

    let root = config
        .as_object_mut()
        .context("existing sing-box config must be a JSON object")?;
    let outbounds = root
        .entry("outbounds")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config outbounds must be an array")?;
    let preferred_selector_tag =
        preferred_existing_tag(outbounds, SELECTOR_TAG_ALIASES, DEFAULT_SELECTOR_TAG);
    let carrier_outbound_tag = if outbounds.iter().any(|outbound| {
        outbound.get("tag").and_then(Value::as_str) == Some(preferred_selector_tag.as_str())
    }) {
        preferred_selector_tag
    } else if let Some(tag) = outbounds
        .iter()
        .find(|outbound| {
            outbound
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "selector" | "urltest"))
        })
        .and_then(|outbound| outbound.get("tag").and_then(Value::as_str))
    {
        tag.to_string()
    } else {
        let direct_tag = preferred_existing_tag(outbounds, DIRECT_TAG_ALIASES, DEFAULT_DIRECT_TAG);
        if outbounds.iter().any(|outbound| {
            outbound.get("tag").and_then(Value::as_str) == Some(direct_tag.as_str())
        }) {
            direct_tag
        } else if let Some(tag) = outbounds
            .iter()
            .find(|outbound| is_replaceable_node_outbound(outbound))
            .and_then(|outbound| outbound.get("tag").and_then(Value::as_str))
        {
            tag.to_string()
        } else {
            upsert_special_outbound(
                outbounds,
                &direct_tag,
                || json!({ "type": "direct", "tag": direct_tag.clone() }),
                |value| ensure_string_field(value, "type", "direct"),
            )?;
            direct_tag
        }
    };
    let route = root
        .entry("route")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing config route must be an object")?;
    let rules = route
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config route.rules must be an array")?;
    let desired_index = rules
        .iter()
        .position(|rule| rule_references_rule_set(rule, DEFAULT_BYPASS_RULE_SET_TAG))
        .or_else(|| {
            rules
                .iter()
                .position(is_dns_hijack_rule)
                .map(|index| index + 1)
        })
        .unwrap_or(0);

    let existing_index = rules.iter().position(|rule| {
        let object = match rule.as_object() {
            Some(object) => object,
            None => return false,
        };
        let only_carrier_fields = object
            .keys()
            .all(|key| matches!(key.as_str(), "action" | "domain" | "outbound"));
        only_carrier_fields
            && rule
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("route")
                == "route"
            && rule.get("outbound").and_then(Value::as_str) == Some(carrier_outbound_tag.as_str())
            && !rule_string_values(rule, "domain").is_empty()
            && rule_string_values(rule, "domain")
                .iter()
                .any(|domain| carrier_domains.contains(domain))
    });
    if let Some(existing_index) = existing_index {
        let mut merged_domains = rule_string_values(&rules[existing_index], "domain");
        merged_domains.extend(carrier_domains);
        merged_domains.sort();
        merged_domains.dedup();
        rules[existing_index]["domain"] = json!(merged_domains);
        rules[existing_index]["outbound"] = json!(carrier_outbound_tag);
        if existing_index > desired_index {
            let rule = rules.remove(existing_index);
            rules.insert(desired_index, rule);
        }
        return Ok(());
    }

    rules.insert(
        desired_index,
        json!({
            "action": "route",
            "domain": carrier_domains,
            "outbound": carrier_outbound_tag,
        }),
    );
    Ok(())
}

struct PrivateAccessRouteTargets<'a> {
    cidrs: &'a [Ipv4Cidr],
    domains: &'a [String],
    domain_suffixes: &'a [String],
}

struct PrivateAccessManagedTargets<'a> {
    current: PrivateAccessRouteTargets<'a>,
    previous: PrivateAccessRouteTargets<'a>,
    direct_tag: &'a str,
    proxy: Option<SocketAddrV4>,
}

fn rule_matches_private_access_route_targets(
    rule: &Value,
    targets: &PrivateAccessManagedTargets<'_>,
) -> bool {
    let Some(object) = rule.as_object() else {
        return false;
    };
    let domains = rule_string_values(rule, "domain");
    let domain_suffixes = rule_string_values(rule, "domain_suffix");
    let matches_domains = (domains == targets.current.domains
        && domain_suffixes == targets.current.domain_suffixes)
        || (domains == targets.previous.domains
            && domain_suffixes == targets.previous.domain_suffixes);

    if rule.get("action").and_then(Value::as_str) == Some("resolve") {
        let only_managed_fields = object.keys().all(|key| {
            matches!(
                key.as_str(),
                "action" | "server" | "strategy" | "disable_cache" | "domain" | "domain_suffix"
            )
        });
        return only_managed_fields
            && rule.get("server").and_then(Value::as_str) == Some(PRIVATE_ACCESS_SYSTEM_DNS_TAG)
            && matches_domains;
    }
    if rule.get("action").and_then(Value::as_str) != Some("route")
        || rule.get("outbound").and_then(Value::as_str) != Some(targets.direct_tag)
    {
        return false;
    }
    let only_managed_fields = object.keys().all(|key| {
        matches!(
            key.as_str(),
            "action"
                | "outbound"
                | "override_address"
                | "override_port"
                | "port"
                | "ip_cidr"
                | "domain"
                | "domain_suffix"
        )
    });
    if !only_managed_fields {
        return false;
    }
    let is_bridge_override =
        rule.get("override_address").is_some() && rule.get("override_port").is_some();
    if targets.proxy.is_some() && !is_bridge_override {
        return false;
    }
    let cidrs = rule_ip_cidrs(rule);
    let matches_profile_cidrs = !cidrs.is_empty()
        && cidrs.iter().any(|rule_cidr| {
            targets
                .current
                .cidrs
                .iter()
                .chain(targets.previous.cidrs)
                .any(|profile_cidr| rule_cidr.overlaps(*profile_cidr))
        });
    let replaces_bridge_with_tun = targets.proxy.is_none()
        && rule.get("override_address").is_some()
        && rule.get("override_port").is_some()
        && !cidrs.is_empty()
        && cidrs.iter().all(|rule_cidr| {
            targets
                .current
                .cidrs
                .iter()
                .any(|target_cidr| rule_cidr.overlaps(*target_cidr))
        });
    ((is_bridge_override && matches_profile_cidrs)
        || (!is_bridge_override
            && (cidrs == targets.current.cidrs || cidrs == targets.previous.cidrs)
            && !cidrs.is_empty()))
        || replaces_bridge_with_tun
        || matches_domains && (!domains.is_empty() || !domain_suffixes.is_empty())
}

fn ensure_private_access_system_dns(root: &mut serde_json::Map<String, Value>) -> Result<()> {
    let dns = root
        .entry("dns")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing config dns must be an object")?;
    let servers = dns
        .entry("servers")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("existing config dns.servers must be an array")?;
    if let Some(server) = servers.iter_mut().find(|server| {
        server.get("tag").and_then(Value::as_str) == Some(PRIVATE_ACCESS_SYSTEM_DNS_TAG)
    }) {
        let server = server
            .as_object_mut()
            .context("private access DNS server must be an object")?;
        server.insert("type".to_string(), Value::String("local".to_string()));
    } else {
        servers.push(json!({
            "type": "local",
            "tag": PRIVATE_ACCESS_SYSTEM_DNS_TAG,
        }));
    }
    Ok(())
}

fn normalize_private_access_domains(values: &[String]) -> Vec<String> {
    let mut domains = values
        .iter()
        .filter_map(|value| {
            let value = value
                .trim()
                .trim_start_matches("*.")
                .trim_start_matches('.')
                .trim_end_matches('.')
                .to_ascii_lowercase();
            (!value.is_empty()
                && value.len() <= 253
                && value.parse::<std::net::IpAddr>().is_err()
                && value.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                }))
            .then_some(value)
        })
        .collect::<Vec<_>>();
    domains.sort();
    domains.dedup();
    domains
}

fn set_private_access_domain_matchers(
    rule: &mut Value,
    domains: &[String],
    domain_suffixes: &[String],
) {
    let object = rule
        .as_object_mut()
        .expect("private access rule is a JSON object");
    if !domains.is_empty() {
        object.insert("domain".to_string(), json!(domains));
    }
    if !domain_suffixes.is_empty() {
        object.insert("domain_suffix".to_string(), json!(domain_suffixes));
    }
}

fn rule_string_values(rule: &Value, field: &str) -> Vec<String> {
    match rule.get(field) {
        Some(Value::String(value)) => vec![value.to_ascii_lowercase()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_ascii_lowercase)
            .collect(),
        _ => Vec::new(),
    }
}

fn rule_ip_cidrs(rule: &Value) -> Vec<Ipv4Cidr> {
    match rule.get("ip_cidr") {
        Some(Value::String(value)) => Ipv4Cidr::parse(value).into_iter().collect(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|value| Ipv4Cidr::parse(value).ok())
            .collect(),
        _ => Vec::new(),
    }
}

fn hillstone_route_insert_index(rules: &[Value]) -> usize {
    // Sing-box already sees user traffic through its normal mixed/system-proxy inbound.
    // This rule keeps that single user-facing entry point and rewrites the matched
    // internal host to the local Hillstone ESP bridge without a port matcher. The bridge
    // still receives the original HTTP Host/authority, so one host-level rule covers
    // services like 10011 and 8099 instead of requiring fragile per-port entries.
    rules
        .iter()
        .position(|rule| {
            rule.get("ip_is_private").and_then(Value::as_bool) == Some(true)
                || rule.get("clash_mode").is_some()
        })
        .unwrap_or_else(|| {
            rules
                .iter()
                .rposition(|existing| existing.get("action").is_some())
                .map(|index| index + 1)
                .unwrap_or(0)
        })
}

fn rule_references_rule_set(rule: &Value, tag: &str) -> bool {
    match rule.get("rule_set") {
        Some(Value::String(value)) => value == tag,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(tag)),
        _ => false,
    }
}

fn rule_matches_cgnat_overlay_cidr(rule: &Value) -> bool {
    rule_matches_ip_cidr(rule, CGNAT_OVERLAY_CIDR)
}

fn rule_matches_ip_cidr(rule: &Value, cidr: &str) -> bool {
    match rule.get("ip_cidr") {
        Some(Value::String(value)) => value == cidr,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(cidr)),
        _ => false,
    }
}

fn set_rule_outbound(rule: &mut Value, direct_tag: &str) {
    let Some(object) = rule.as_object_mut() else {
        return;
    };
    object.insert(
        "outbound".to_string(),
        Value::String(direct_tag.to_string()),
    );
}

fn is_dns_hijack_rule(rule: &Value) -> bool {
    rule.get("action").and_then(Value::as_str) == Some("hijack-dns")
}

fn is_replaceable_node_outbound(outbound: &Value) -> bool {
    let outbound_type = outbound
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    !matches!(
        outbound_type,
        "selector" | "urltest" | "direct" | "block" | "dns"
    )
}

fn is_metadata_node_tag(tag: &str) -> bool {
    tag.starts_with("剩余流量")
        || tag.starts_with("距离下次重置剩余")
        || tag.starts_with("套餐到期")
        || tag.contains("官网")
        || tag.contains("刷新订阅")
        || tag.contains("如遇不可用请访问")
        || tag.contains("请更换客户端")
        || tag.contains("直连地址")
        || tag.contains("TG群")
        || tag.contains("邀请好友")
}

fn upsert_special_outbound<F, G>(
    outbounds: &mut Vec<Value>,
    tag: &str,
    build_default: F,
    update: G,
) -> Result<()>
where
    F: FnOnce() -> Value,
    G: FnOnce(&mut Value),
{
    if let Some(existing) = find_outbound_by_tag_mut(outbounds, tag) {
        update(existing);
    } else {
        outbounds.push(build_default());
    }
    Ok(())
}

fn upsert_tagged_outbound(outbounds: &mut Vec<Value>, outbound: Value) -> Result<()> {
    let tag = outbound_tag(&outbound)?.to_string();
    if let Some(existing) = find_outbound_by_tag_mut(outbounds, &tag) {
        *existing = outbound;
    } else {
        outbounds.push(outbound);
    }
    Ok(())
}

fn find_outbound_by_tag_mut<'a>(outbounds: &'a mut [Value], tag: &str) -> Option<&'a mut Value> {
    outbounds.iter_mut().find(|value| {
        value
            .get("tag")
            .and_then(Value::as_str)
            .is_some_and(|current| current == tag)
    })
}

fn outbound_tag(outbound: &Value) -> Result<&str> {
    outbound
        .get("tag")
        .and_then(Value::as_str)
        .context("converted outbound is missing string tag")
}

fn preferred_existing_tag(outbounds: &[Value], aliases: &[&str], preferred: &str) -> String {
    outbounds
        .iter()
        .filter_map(|value| value.get("tag").and_then(Value::as_str))
        .find(|tag| aliases.iter().any(|alias| alias == tag))
        .unwrap_or(preferred)
        .to_string()
}

fn collect_tags(outbounds: &[Value]) -> Vec<String> {
    outbounds
        .iter()
        .filter_map(|value| value.get("tag").and_then(Value::as_str))
        .map(ToString::to_string)
        .collect()
}

fn with_leading_members(first_members: &[&str], tags: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut members = Vec::with_capacity(tags.len() + first_members.len());
    for member in first_members {
        if seen.insert((*member).to_string()) {
            members.push((*member).to_string());
        }
    }
    for tag in tags {
        if seen.insert(tag.clone()) {
            members.push(tag.clone());
        }
    }
    members
}

fn set_outbound_members(outbound: &mut Value, members: &[String]) {
    let Some(object) = outbound.as_object_mut() else {
        return;
    };
    object.insert(
        "outbounds".to_string(),
        Value::Array(
            members
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>(),
        ),
    );
}

fn merge_outbound_members(outbound: &mut Value, new_members: &[String]) {
    let Some(object) = outbound.as_object_mut() else {
        return;
    };

    let mut merged = BTreeSet::new();
    let entry = object
        .entry("outbounds")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(existing) = entry.as_array_mut() else {
        return;
    };

    let mut values = Vec::new();
    for value in existing.iter() {
        if let Some(member) = value.as_str()
            && merged.insert(member.to_string())
        {
            values.push(Value::String(member.to_string()));
        }
    }
    for member in new_members {
        if merged.insert(member.clone()) {
            values.push(Value::String(member.clone()));
        }
    }
    *existing = values;
}

fn ensure_string_field(outbound: &mut Value, key: &str, value: &str) {
    let Some(object) = outbound.as_object_mut() else {
        return;
    };
    object
        .entry(key.to_string())
        .or_insert_with(|| Value::String(value.to_string()));
}

fn set_bool_field(outbound: &mut Value, key: &str, value: bool) {
    let Some(object) = outbound.as_object_mut() else {
        return;
    };
    object.insert(key.to_string(), Value::Bool(value));
}

#[cfg(test)]
mod tests {
    use super::{
        DefaultConfigOptions, HillstoneRouteTableOptions, PRIVATE_ACCESS_SYSTEM_DNS_TAG,
        PrivateAccessRouteTableOptions, ProviderNodeSet, RouteAutoDetectInterfaceState,
        TAILSCALE_DNS_TAG, TAILSCALE_ENDPOINT_TAG, TAILSCALE_IPV6_CIDR, TailscaleConfigOptions,
        TailscaleConfigState, build_default_config, build_default_config_with_options,
        build_full_config_with_provider_node_sets, config_has_china_ip_routing,
        config_has_internet_tun_inbound, default_tun_inbound,
        ensure_bypass_rule_set_file_for_config, ensure_hillstone_route_table,
        ensure_private_access_route_table, inspect_tailscale_config, inspect_tun_config,
        is_managed_tailscale_control_plane_dns_rule, lock_config_mutation,
        merge_into_existing_config, run_private_access_route_table_config,
        run_private_access_tun_baseline_config, set_china_ip_routing, set_internet_tun_mode,
        set_tailscale_config,
    };
    use crate::defaults::{DEFAULT_BYPASS_RULE_SET_PATH, DEFAULT_BYPASS_RULE_SET_TAG};
    use serde_json::{Value, json};
    use std::fs;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn default_full_config_contains_selector_and_imported_nodes() {
        let config = build_default_config(vec![json!({
            "type": "trojan",
            "tag": "node-a",
            "server": "example.com",
            "server_port": 443,
            "password": "secret",
        })]);

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        let inbounds = config["inbounds"].as_array().expect("inbounds array");
        assert!(!inbounds.iter().any(|value| value["type"] == "tun"));
        assert!(inbounds.iter().any(|value| value["type"] == "mixed"));
        assert!(outbounds.iter().any(|value| value["tag"] == "手动选择"));
        assert!(outbounds.iter().any(|value| value["tag"] == "自动选择"));
        assert!(outbounds.iter().any(|value| value["tag"] == "广告路由"));
        assert!(outbounds.iter().any(|value| value["tag"] == "node-a"));
        let select = outbounds
            .iter()
            .find(|value| value["tag"] == "手动选择")
            .expect("manual selector");
        let members = select["outbounds"].as_array().expect("selector members");
        assert!(!members.contains(&Value::String("国内直连".to_string())));
        assert_eq!(config["dns"]["servers"][0]["type"], "tls");
        assert_eq!(config["dns"]["servers"][0]["server"], "223.6.6.6");
        assert!(config["dns"]["servers"][0].get("detour").is_none());
        let mixed = inbounds
            .iter()
            .find(|value| value["type"] == "mixed")
            .expect("mixed inbound");
        assert_eq!(mixed["listen_port"], 6780);
        assert_eq!(
            config["route"]["default_domain_resolver"]["server"],
            "local"
        );
        assert_eq!(config["route"]["rules"][0]["action"], "hijack-dns");
        assert_eq!(
            config["route"]["rules"].as_array().expect("route rules")[1]["domain_suffix"],
            json!(["airtcp.me", "airtcp.com", "airapp.link", "mailrelay.us"])
        );
        assert!(
            config["route"]["rules"].as_array().expect("route rules")[2]["rule_set"]
                == "sing-box-tui-bypass"
        );
        assert_eq!(
            config["route"]["rules"].as_array().expect("route rules")[3]["ip_cidr"],
            json!(["100.64.0.0/10"])
        );
        assert_eq!(
            config["route"]["rules"].as_array().expect("route rules")[3]["outbound"],
            "国内直连"
        );
        assert!(
            config["route"]["rule_set"]
                .as_array()
                .expect("rule sets")
                .iter()
                .any(|value| value["tag"] == "sing-box-tui-bypass"
                    && value["type"] == "local"
                    && value["format"] == "source")
        );
        assert_eq!(
            config["experimental"]["clash_api"]["external_controller"],
            "0.0.0.0:9992"
        );
        assert!(!value_references(&config, "geoip-cn"));
        assert!(!value_references(&config, "geosite-cn"));
        assert!(!value_references(&config, "geosite-geolocation-cn"));
        assert!(!value_references(&config, "geosite-geolocation-!cn"));
        assert!(!value_references(&config, "AdGuardSDNSFilter"));
        assert!(!value_references(&config, "meta-rules-dat"));
        assert!(config["route"].get("final").is_none());
    }

    #[test]
    fn default_full_config_can_include_geosite_rules() {
        let config = build_default_config_with_options(
            vec![json!({
                "type": "trojan",
                "tag": "node-a",
                "server": "example.com",
                "server_port": 443,
                "password": "secret",
            })],
            DefaultConfigOptions {
                include_geosite_rules: true,
                include_tun_mode: false,
            },
        );

        for tag in [
            "geoip-cn",
            "geosite-cn",
            "geosite-geolocation-cn",
            "geosite-geolocation-!cn",
            "AdGuardSDNSFilter",
        ] {
            assert!(value_references(&config, tag), "missing {tag}");
        }
        assert!(value_references(&config, "meta-rules-dat"));
    }

    #[test]
    fn default_full_config_can_include_tun_mode() {
        let config = build_default_config_with_options(
            vec![json!({
                "type": "trojan",
                "tag": "node-a",
                "server": "example.com",
                "server_port": 443,
                "password": "secret",
            })],
            DefaultConfigOptions {
                include_geosite_rules: false,
                include_tun_mode: true,
            },
        );

        let inbounds = config["inbounds"].as_array().expect("inbounds array");
        let tun = inbounds
            .iter()
            .find(|value| value["type"] == "tun")
            .expect("tun inbound");
        assert_eq!(tun["tag"], "tun-in");
        assert_eq!(tun["address"], json!(["172.19.0.1/30"]));
        assert_eq!(tun["mtu"], 9000);
        assert_eq!(tun["auto_route"], true);
        assert_eq!(tun["strict_route"], true);
        assert_eq!(tun["stack"], "mixed");
        assert_eq!(tun["endpoint_independent_nat"], true);
        assert!(tun.get("auto_redirect").is_none());
        assert_eq!(config["route"]["auto_detect_interface"], true);
    }

    #[test]
    fn default_config_preserves_existing_connections_on_selector_changes() {
        let config = build_default_config(vec![json!({
            "type": "trojan",
            "tag": "node-a",
            "server": "example.com",
            "server_port": 443,
            "password": "secret",
        })]);

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        let selector = outbounds
            .iter()
            .find(|value| value["tag"] == "手动选择")
            .expect("manual selector");
        let auto = outbounds
            .iter()
            .find(|value| value["tag"] == "自动选择")
            .expect("auto selector");
        assert_eq!(selector["interrupt_exist_connections"], false);
        assert_eq!(auto["interrupt_exist_connections"], false);
    }

    #[test]
    fn creates_missing_bypass_rule_set_next_to_config() {
        let config_path = temp_config_path("bypass-rule-set");
        let bypass_path = config_path.with_file_name(DEFAULT_BYPASS_RULE_SET_PATH);
        let _ = fs::remove_file(&bypass_path);

        let created = ensure_bypass_rule_set_file_for_config(&config_path)
            .expect("bypass rule-set creation succeeds");

        assert_eq!(created, Some(bypass_path.clone()));
        let text = fs::read_to_string(&bypass_path).expect("read bypass rule-set");
        let value: Value = serde_json::from_str(&text).expect("parse bypass rule-set");
        assert_eq!(value["version"], Value::from(1));
        assert_eq!(value["rules"], Value::Array(Vec::new()));

        let second = ensure_bypass_rule_set_file_for_config(&config_path)
            .expect("existing bypass rule-set is preserved");
        assert_eq!(second, None);

        let _ = fs::remove_file(config_path);
        let _ = fs::remove_file(bypass_path);
    }

    #[test]
    fn hillstone_route_overrides_internal_host_to_local_bridge() {
        let mut config = build_default_config(Vec::new());

        ensure_hillstone_route_table(
            &mut config,
            HillstoneRouteTableOptions {
                cidrs: vec!["10.1.126.5/32".to_string()],
                proxy: "127.0.0.1:18080".parse().expect("proxy parses"),
            },
        )
        .expect("route is inserted");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        let hillstone_index = rules
            .iter()
            .position(|rule| rule["override_address"] == "127.0.0.1")
            .expect("hillstone route exists");
        let private_index = rules
            .iter()
            .position(|rule| rule["ip_is_private"] == true)
            .expect("private direct route exists");
        assert!(
            hillstone_index < private_index,
            "Hillstone override must run before generic private direct routing"
        );
        let rule = &rules[hillstone_index];
        assert_eq!(rule["action"], "route");
        assert_eq!(rule["ip_cidr"], json!(["10.1.126.5/32"]));
        assert!(rule.get("port").is_none());
        assert_eq!(rule["outbound"], "国内直连");
        assert_eq!(rule["override_address"], "127.0.0.1");
        assert_eq!(rule["override_port"], 18080);
    }

    #[test]
    fn private_access_route_table_overrides_pushed_cidrs_without_port_matcher() {
        let mut config = build_default_config(Vec::new());

        ensure_private_access_route_table(
            &mut config,
            PrivateAccessRouteTableOptions {
                profile_id: "hillstone".to_string(),
                cidrs: vec!["10.1.0.0/16".to_string()],
                domains: Vec::new(),
                domain_suffixes: Vec::new(),
                previous_cidrs: Vec::new(),
                previous_domains: Vec::new(),
                previous_domain_suffixes: Vec::new(),
                carrier_domains: Vec::new(),
                proxy: Some("127.0.0.1:18080".parse().expect("proxy parses")),
            },
        )
        .expect("private access route is inserted");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        let rule = rules
            .iter()
            .find(|rule| rule["ip_cidr"] == json!(["10.1.0.0/16"]))
            .expect("private access route exists");
        assert_eq!(rule["action"], "route");
        assert!(rule.get("port").is_none());
        assert_eq!(rule["override_address"], "127.0.0.1");
        assert_eq!(rule["override_port"], 18080);
    }

    #[test]
    fn private_access_route_update_preserves_overlapping_custom_direct_rule() {
        let mut config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "rules": [{
                "action": "route",
                "ip_cidr": ["10.1.0.0/16", "192.168.0.0/16"],
                "protocol": "tcp",
                "outbound": "direct"
            }] }
        });

        ensure_private_access_route_table(
            &mut config,
            PrivateAccessRouteTableOptions {
                profile_id: "hillstone".to_string(),
                cidrs: vec!["10.1.0.0/16".to_string()],
                domains: Vec::new(),
                domain_suffixes: Vec::new(),
                previous_cidrs: Vec::new(),
                previous_domains: Vec::new(),
                previous_domain_suffixes: Vec::new(),
                carrier_domains: Vec::new(),
                proxy: Some("127.0.0.1:18080".parse().expect("proxy parses")),
            },
        )
        .expect("route update succeeds");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        assert!(rules.iter().any(|rule| {
            rule["ip_cidr"] == json!(["10.1.0.0/16", "192.168.0.0/16"]) && rule["protocol"] == "tcp"
        }));
    }

    #[test]
    fn private_access_route_update_removes_disjoint_previous_managed_route() {
        let mut config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "rules": [{
                "action": "route",
                "ip_cidr": ["10.1.0.0/16"],
                "outbound": "direct",
                "override_address": "127.0.0.1",
                "override_port": 18080
            }] }
        });

        ensure_private_access_route_table(
            &mut config,
            PrivateAccessRouteTableOptions {
                profile_id: "hillstone".to_string(),
                cidrs: vec!["10.2.0.0/16".to_string()],
                domains: Vec::new(),
                domain_suffixes: Vec::new(),
                previous_cidrs: vec!["10.1.0.0/16".to_string()],
                previous_domains: Vec::new(),
                previous_domain_suffixes: Vec::new(),
                carrier_domains: Vec::new(),
                proxy: Some("127.0.0.1:18080".parse().expect("proxy parses")),
            },
        )
        .expect("route update succeeds");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        assert!(
            !rules
                .iter()
                .any(|rule| rule["ip_cidr"] == json!(["10.1.0.0/16"]))
        );
        assert!(
            rules
                .iter()
                .any(|rule| rule["ip_cidr"] == json!(["10.2.0.0/16"]))
        );
    }

    #[test]
    fn private_access_domain_update_preserves_other_profiles_resolve_rule() {
        let mut config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "rules": [{
                "action": "resolve",
                "server": PRIVATE_ACCESS_SYSTEM_DNS_TAG,
                "strategy": "ipv4_only",
                "disable_cache": true,
                "domain": ["profile-a.internal"]
            }] }
        });

        ensure_private_access_route_table(
            &mut config,
            PrivateAccessRouteTableOptions {
                profile_id: "profile-b".to_string(),
                cidrs: Vec::new(),
                domains: vec!["profile-b.internal".to_string()],
                domain_suffixes: Vec::new(),
                previous_cidrs: Vec::new(),
                previous_domains: Vec::new(),
                previous_domain_suffixes: Vec::new(),
                carrier_domains: Vec::new(),
                proxy: Some("127.0.0.1:18081".parse().expect("proxy parses")),
            },
        )
        .expect("domain update succeeds");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        assert!(rules.iter().any(|rule| {
            rule["action"] == "resolve" && rule["domain"] == json!(["profile-a.internal"])
        }));
        assert!(rules.iter().any(|rule| {
            rule["action"] == "resolve" && rule["domain"] == json!(["profile-b.internal"])
        }));
    }

    #[test]
    fn private_access_tun_route_replaces_bridge_override_with_direct_route() {
        let mut config = json!({
            "outbounds": [{
                "type": "direct",
                "tag": "direct"
            }],
            "route": {
                "rules": [{
                    "action": "route",
                    "ip_cidr": ["10.1.0.0/16", "10.255.0.0/24"],
                    "outbound": "direct",
                    "override_address": "127.0.0.1",
                    "override_port": 18080
                }, {
                    "ip_is_private": true,
                    "outbound": "direct"
                }]
            }
        });

        ensure_private_access_route_table(
            &mut config,
            PrivateAccessRouteTableOptions {
                profile_id: "hillstone".to_string(),
                cidrs: vec![
                    "10.1.0.0/16".to_string(),
                    "10.255.0.0/24".to_string(),
                    "10.253.0.0/24".to_string(),
                ],
                domains: Vec::new(),
                domain_suffixes: Vec::new(),
                previous_cidrs: Vec::new(),
                previous_domains: Vec::new(),
                previous_domain_suffixes: Vec::new(),
                carrier_domains: Vec::new(),
                proxy: None,
            },
        )
        .expect("TUN direct route is inserted");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        let private_access_rules = rules
            .iter()
            .filter(|rule| rule["ip_cidr"].is_array())
            .collect::<Vec<_>>();
        assert_eq!(private_access_rules.len(), 1);
        assert_eq!(config["route"]["auto_detect_interface"], false);
        let rule = private_access_rules[0];
        assert_eq!(
            rule["ip_cidr"],
            json!(["10.1.0.0/16", "10.255.0.0/24", "10.253.0.0/24"])
        );
        assert_eq!(rule["outbound"], "direct");
        assert!(rule.get("override_address").is_none());
        assert!(rule.get("override_port").is_none());
    }

    #[test]
    fn private_access_domains_resolve_with_system_dns_before_direct_routing() {
        let mut config = json!({
            "dns": { "servers": [] },
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "selector", "tag": "select", "outbounds": ["node-a"] },
                {
                    "type": "trojan",
                    "tag": "node-a",
                    "server": "proxy.example.com",
                    "server_port": 443,
                    "password": "secret"
                },
                {
                    "type": "trojan",
                    "tag": "unrelated-node",
                    "server": "unrelated.example.com",
                    "server_port": 443,
                    "password": "secret"
                }
            ],
            "route": {
                "rules": [
                    {
                        "type": "logical",
                        "mode": "or",
                        "rules": [{ "protocol": "dns" }, { "port": 53 }],
                        "action": "hijack-dns"
                    },
                    { "rule_set": DEFAULT_BYPASS_RULE_SET_TAG, "outbound": "direct" },
                    { "ip_is_private": true, "outbound": "direct" }
                ]
            }
        });

        ensure_private_access_route_table(
            &mut config,
            PrivateAccessRouteTableOptions {
                profile_id: "sonicwall".to_string(),
                cidrs: vec!["192.168.0.0/16".to_string()],
                domains: vec!["Service.Hundsun.com".to_string()],
                domain_suffixes: vec!["*.Hundsun.COM.".to_string()],
                previous_cidrs: Vec::new(),
                previous_domains: Vec::new(),
                previous_domain_suffixes: Vec::new(),
                carrier_domains: vec!["sslvpn.hundsun.com".to_string()],
                proxy: None,
            },
        )
        .expect("SonicWall domain routes are inserted");

        let dns_servers = config["dns"]["servers"]
            .as_array()
            .expect("DNS servers array");
        assert!(dns_servers.iter().any(|server| {
            server["type"] == "local" && server["tag"] == PRIVATE_ACCESS_SYSTEM_DNS_TAG
        }));

        let rules = config["route"]["rules"].as_array().expect("route rules");
        let carrier_index = rules
            .iter()
            .position(|rule| rule["domain"] == json!(["sslvpn.hundsun.com"]))
            .expect("SonicWall carrier route");
        let bypass_index = rules
            .iter()
            .position(|rule| rule["rule_set"] == DEFAULT_BYPASS_RULE_SET_TAG)
            .expect("generic bypass route");
        let resolve_index = rules
            .iter()
            .position(|rule| rule["action"] == "resolve")
            .expect("private access resolve rule");
        let domain_index = rules
            .iter()
            .position(|rule| {
                rule["action"] == "route" && rule["domain"] == json!(["service.hundsun.com"])
            })
            .expect("private access domain route");
        let cidr_index = rules
            .iter()
            .position(|rule| rule["ip_cidr"] == json!(["192.168.0.0/16"]))
            .expect("private access CIDR route");
        assert!(carrier_index < bypass_index);
        assert!(carrier_index < resolve_index);
        assert_eq!(rules[carrier_index]["outbound"], "select");
        assert!(resolve_index < domain_index);
        assert!(domain_index < cidr_index);
        assert_eq!(
            rules[resolve_index]["server"],
            PRIVATE_ACCESS_SYSTEM_DNS_TAG
        );
        assert_eq!(rules[resolve_index]["disable_cache"], true);
        assert_eq!(
            rules[resolve_index]["domain_suffix"],
            json!(["hundsun.com"])
        );
        assert_eq!(rules[domain_index]["outbound"], "direct");
        assert_eq!(config["route"]["auto_detect_interface"], false);
    }

    #[test]
    fn private_access_carrier_preserves_custom_direct_rule() {
        let mut config = json!({
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "selector", "tag": "select", "outbounds": ["direct"] }
            ],
            "route": {
                "rules": [{
                    "action": "route",
                    "domain": ["sslvpn.hundsun.com", "keep-direct.example"],
                    "outbound": "direct"
                }]
            }
        });

        ensure_private_access_route_table(
            &mut config,
            PrivateAccessRouteTableOptions {
                profile_id: "sonicwall".to_string(),
                cidrs: Vec::new(),
                domains: Vec::new(),
                domain_suffixes: Vec::new(),
                previous_cidrs: Vec::new(),
                previous_domains: Vec::new(),
                previous_domain_suffixes: Vec::new(),
                carrier_domains: vec!["sslvpn.hundsun.com".to_string()],
                proxy: None,
            },
        )
        .expect("private access carrier is inserted");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        assert!(rules.iter().any(|rule| {
            rule["domain"] == json!(["sslvpn.hundsun.com", "keep-direct.example"])
                && rule["outbound"] == "direct"
        }));
        assert!(rules.iter().any(|rule| {
            rule["domain"] == json!(["sslvpn.hundsun.com"]) && rule["outbound"] == "select"
        }));
    }

    #[test]
    fn private_access_carrier_uses_existing_direct_when_selector_is_absent() {
        let mut config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "rules": [] }
        });

        ensure_private_access_route_table(
            &mut config,
            PrivateAccessRouteTableOptions {
                profile_id: "sonicwall".to_string(),
                cidrs: Vec::new(),
                domains: Vec::new(),
                domain_suffixes: Vec::new(),
                previous_cidrs: Vec::new(),
                previous_domains: Vec::new(),
                previous_domain_suffixes: Vec::new(),
                carrier_domains: vec!["sslvpn.hundsun.com".to_string()],
                proxy: None,
            },
        )
        .expect("private access carrier is inserted");

        assert!(config["route"]["rules"].as_array().is_some_and(|rules| {
            rules.iter().any(|rule| {
                rule["domain"] == json!(["sslvpn.hundsun.com"]) && rule["outbound"] == "direct"
            })
        }));
    }

    #[test]
    fn private_access_route_config_reports_only_real_changes() {
        let path = temp_config_path("private-access-change-detection");
        let config = build_default_config(Vec::new());
        fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("temporary config is written");
        let options = PrivateAccessRouteTableOptions {
            profile_id: "sonicwall".to_string(),
            cidrs: vec!["10.22.0.0/16".to_string()],
            domains: Vec::new(),
            domain_suffixes: Vec::new(),
            previous_cidrs: Vec::new(),
            previous_domains: Vec::new(),
            previous_domain_suffixes: Vec::new(),
            carrier_domains: Vec::new(),
            proxy: None,
        };

        assert!(
            run_private_access_route_table_config(&path, None, true, options.clone())
                .expect("first update succeeds")
        );
        assert!(
            !run_private_access_route_table_config(&path, None, true, options)
                .expect("idempotent update succeeds")
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn private_access_tun_baseline_merges_carriers_without_rule_churn() {
        let path = temp_config_path("private-access-tun-baseline");
        let config = json!({
            "dns": { "servers": [] },
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "selector", "tag": "select", "outbounds": ["direct"] }
            ],
            "route": {
                "auto_detect_interface": true,
                "rules": [
                    { "action": "hijack-dns", "protocol": "dns" },
                    {
                        "action": "route",
                        "rule_set": [DEFAULT_BYPASS_RULE_SET_TAG],
                        "outbound": "direct"
                    }
                ]
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("temporary config is written");

        let carriers = vec![
            "sslvpn.hundsun.com".to_string(),
            "sslvpn.geovisearth.com".to_string(),
        ];
        assert!(
            run_private_access_tun_baseline_config(&path, true, &carriers)
                .expect("first baseline update succeeds")
        );
        let first = fs::read_to_string(&path).expect("updated config reads");
        assert!(
            !run_private_access_tun_baseline_config(&path, true, &carriers)
                .expect("repeated baseline update succeeds")
        );
        assert_eq!(
            fs::read_to_string(&path).expect("idempotent config reads"),
            first
        );

        assert!(
            !run_private_access_tun_baseline_config(
                &path,
                true,
                &["sslvpn.hundsun.com".to_string()],
            )
            .expect("subset carrier update succeeds")
        );
        let parsed: Value = serde_json::from_str(&first).expect("updated config parses");
        assert_eq!(parsed["route"]["auto_detect_interface"], false);
        assert!(
            parsed["route"]["rules"]
                .as_array()
                .is_some_and(|rules| rules.iter().any(|rule| {
                    rule["domain"] == json!(["sslvpn.geovisearth.com", "sslvpn.hundsun.com"])
                        && rule["outbound"] == "select"
                }))
        );
        assert!(parsed["route"]["rules"].as_array().is_some_and(|rules| {
            rules
                .iter()
                .any(|rule| rule["ip_is_private"] == true && rule["outbound"] == "direct")
        }));
        assert!(
            parsed["dns"]["servers"]
                .as_array()
                .is_some_and(|servers| servers.iter().any(|server| {
                    server["tag"] == PRIVATE_ACCESS_SYSTEM_DNS_TAG && server["type"] == "local"
                }))
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn private_access_baseline_keeps_internet_tun_route_loop_protection() {
        let path = temp_config_path("private-access-with-internet-tun");
        let config = json!({
            "dns": { "servers": [] },
            "inbounds": [default_tun_inbound()],
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "selector", "tag": "select", "outbounds": ["direct"] }
            ],
            "route": {
                "auto_detect_interface": false,
                "rules": [{ "action": "hijack-dns", "protocol": "dns" }]
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("config serializes"),
        )
        .expect("temporary config is written");

        assert!(
            run_private_access_tun_baseline_config(&path, true, &[])
                .expect("baseline update succeeds")
        );
        let parsed: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("updated config reads"))
                .expect("updated config parses");
        assert_eq!(parsed["route"]["auto_detect_interface"], true);
        assert!(
            !run_private_access_tun_baseline_config(&path, true, &[])
                .expect("repeated baseline update succeeds")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn private_access_baseline_preserves_owned_or_custom_tun_route_policy() {
        for (name, inbounds, auto_detect_interface) in [
            ("owned-enabled", json!([]), Some(true)),
            ("owned-missing", json!([]), None),
            (
                "custom-tun",
                json!([{
                    "type": "tun",
                    "tag": "custom-tun",
                    "address": ["172.20.0.1/30"],
                    "auto_route": true
                }]),
                Some(true),
            ),
        ] {
            let path = temp_config_path(name);
            let mut route = json!({ "rules": [] });
            if let Some(value) = auto_detect_interface {
                route["auto_detect_interface"] = Value::Bool(value);
            }
            let config = json!({
                "dns": {
                    "servers": [{
                        "type": "local",
                        "tag": PRIVATE_ACCESS_SYSTEM_DNS_TAG
                    }]
                },
                "inbounds": inbounds,
                "outbounds": [{ "type": "direct", "tag": "direct" }],
                "route": route
            });
            fs::write(
                &path,
                serde_json::to_string_pretty(&config).expect("serializes"),
            )
            .expect("writes config");

            run_private_access_tun_baseline_config(&path, true, &[])
                .expect("baseline update succeeds");
            let parsed: Value =
                serde_json::from_str(&fs::read_to_string(&path).expect("reads config"))
                    .expect("parses config");
            assert_eq!(
                parsed["route"]
                    .get("auto_detect_interface")
                    .and_then(Value::as_bool),
                auto_detect_interface,
                "{name}"
            );
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn internet_tun_round_trip_survives_private_access_baseline_replay() {
        let path = temp_config_path("internet-tun-private-access-round-trip");
        let config = json!({
            "dns": {
                "servers": [{
                    "type": "local",
                    "tag": PRIVATE_ACCESS_SYSTEM_DNS_TAG
                }]
            },
            "inbounds": [{ "type": "mixed", "listen_port": 6780 }],
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": {
                "auto_detect_interface": true,
                "rules": []
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("serializes"),
        )
        .expect("writes config");

        let enabled = set_internet_tun_mode(&path, true, None).expect("enables Internet TUN");
        run_private_access_tun_baseline_config(&path, true, &[])
            .expect("enabled startup baseline applies");
        assert_eq!(
            inspect_tun_config(&path)
                .expect("enabled config inspects")
                .auto_detect_interface,
            RouteAutoDetectInterfaceState::Enabled
        );

        set_internet_tun_mode(
            &path,
            false,
            Some(enabled.auto_detect_interface_before_enable),
        )
        .expect("disables Internet TUN");
        run_private_access_tun_baseline_config(&path, true, &[])
            .expect("disabled startup baseline replays");
        let disabled = inspect_tun_config(&path).expect("disabled config inspects");
        assert!(!disabled.managed_internet_tun);
        assert_eq!(
            disabled.auto_detect_interface,
            RouteAutoDetectInterfaceState::Enabled
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn hillstone_route_updates_existing_target_without_duplicates() {
        let mut config = json!({
            "outbounds": [{
                "type": "direct",
                "tag": "direct"
            }],
            "route": {
                "rules": [{
                    "action": "route",
                    "ip_cidr": ["10.1.126.5/32"],
                    "port": 10011,
                    "outbound": "direct",
                    "override_address": "127.0.0.1",
                    "override_port": 18080
                }, {
                    "action": "route",
                    "ip_cidr": ["10.1.126.5/32"],
                    "port": 8099,
                    "outbound": "direct",
                    "override_address": "127.0.0.1",
                    "override_port": 18080
                }, {
                    "ip_is_private": true,
                    "outbound": "direct"
                }]
            }
        });

        ensure_hillstone_route_table(
            &mut config,
            HillstoneRouteTableOptions {
                cidrs: vec!["10.1.126.5/32".to_string()],
                proxy: "127.0.0.1:18081".parse().expect("proxy parses"),
            },
        )
        .expect("route is updated");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        let hillstone_rules = rules
            .iter()
            .filter(|rule| rule["ip_cidr"] == json!(["10.1.126.5/32"]))
            .collect::<Vec<_>>();
        assert_eq!(hillstone_rules.len(), 1);
        assert!(hillstone_rules[0].get("port").is_none());
        assert_eq!(hillstone_rules[0]["override_port"], 18081);
        assert_eq!(hillstone_rules[0]["outbound"], "direct");
    }

    #[test]
    fn hillstone_route_table_replaces_covered_host_routes() {
        let mut config = json!({
            "outbounds": [{
                "type": "direct",
                "tag": "direct"
            }],
            "route": {
                "rules": [{
                    "action": "route",
                    "ip_cidr": ["10.1.126.5/32"],
                    "port": 10011,
                    "outbound": "direct",
                    "override_address": "127.0.0.1",
                    "override_port": 18080
                }, {
                    "action": "route",
                    "ip_cidr": ["10.255.0.0/24"],
                    "outbound": "direct",
                    "override_address": "127.0.0.1",
                    "override_port": 18080
                }, {
                    "ip_cidr": ["10.2.0.0/16"],
                    "outbound": "direct"
                }, {
                    "ip_is_private": true,
                    "outbound": "direct"
                }]
            }
        });

        ensure_hillstone_route_table(
            &mut config,
            HillstoneRouteTableOptions {
                cidrs: vec![
                    "10.1.0.0/16".to_string(),
                    "10.255.0.0/24".to_string(),
                    "10.253.0.7/24".to_string(),
                ],
                proxy: "127.0.0.1:18081".parse().expect("proxy parses"),
            },
        )
        .expect("route table is inserted");

        let rules = config["route"]["rules"].as_array().expect("route rules");
        let hillstone_rules = rules
            .iter()
            .filter(|rule| rule.get("override_address").is_some())
            .collect::<Vec<_>>();
        assert_eq!(hillstone_rules.len(), 1);
        let rule = hillstone_rules[0];
        assert_eq!(
            rule["ip_cidr"],
            json!(["10.1.0.0/16", "10.255.0.0/24", "10.253.0.0/24"])
        );
        assert!(rule.get("port").is_none());
        assert_eq!(rule["override_address"], "127.0.0.1");
        assert_eq!(rule["override_port"], 18081);
        assert_eq!(rule["outbound"], "direct");
        assert!(
            rules
                .iter()
                .any(|rule| rule["ip_cidr"] == json!(["10.2.0.0/16"]))
        );

        let hillstone_index = rules
            .iter()
            .position(|rule| rule.get("override_address").is_some())
            .expect("hillstone route exists");
        let private_index = rules
            .iter()
            .position(|rule| rule["ip_is_private"] == true)
            .expect("private direct route exists");
        assert!(hillstone_index < private_index);
    }

    #[test]
    fn hillstone_route_accepts_sing_box_jsonc_config() {
        let mut config = super::parse_sing_box_config_text(
            r#"{
                // sing-box accepts this style, so config editing must too.
                "metadata": {
                    "url": "https://example.com/not//a-comment",
                },
                "outbounds": [{
                    "type": "direct",
                    "tag": "direct",
                },],
                "route": {
                    "rules": [{
                        "ip_is_private": true,
                        "outbound": "direct",
                    },],
                },
            }"#,
        )
        .expect("sing-box JSONC parses");

        ensure_hillstone_route_table(
            &mut config,
            HillstoneRouteTableOptions {
                cidrs: vec!["10.1.126.5/32".to_string()],
                proxy: "127.0.0.1:18080".parse().expect("proxy parses"),
            },
        )
        .expect("route is inserted into JSONC config");

        assert_eq!(
            config["metadata"]["url"],
            "https://example.com/not//a-comment"
        );
        let rules = config["route"]["rules"].as_array().expect("route rules");
        assert!(rules.iter().any(|rule| {
            rule["ip_cidr"] == json!(["10.1.126.5/32"])
                && rule.get("port").is_none()
                && rule["override_port"] == 18080
        }));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn merge_removes_old_default_auto_redirect_tun_inbound() {
        let mut config = json!({
            "inbounds": [{
                "type": "tun",
                "mtu": 9000,
                "address": [
                    "172.19.0.1/30",
                    "2001:0470:f9da:fdfa::1/64"
                ],
                "auto_route": true,
                "auto_redirect": true,
                "strict_route": true,
                "stack": "mixed",
                "endpoint_independent_nat": true
            }, {
                "type": "mixed",
                "listen": "::",
                "listen_port": 6780
            }],
            "outbounds": [{
                "type": "selector",
                "tag": "select",
                "outbounds": ["old-node"]
            }]
        });

        merge_into_existing_config(&mut config, Vec::new(), false).expect("merge succeeds");

        let inbounds = config["inbounds"].as_array().expect("inbounds array");
        assert!(!inbounds.iter().any(|value| value["type"] == "tun"));
        assert!(inbounds.iter().any(|value| value["type"] == "mixed"));
    }

    #[test]
    fn merge_updates_existing_selectors_to_preserve_connections() {
        let mut config = json!({
            "outbounds": [{
                "type": "selector",
                "tag": "select",
                "outbounds": ["old-node"],
                "default": "old-node",
                "interrupt_exist_connections": true
            }, {
                "type": "urltest",
                "tag": "auto",
                "outbounds": ["old-node"],
                "url": "https://www.gstatic.com/generate_204",
                "interval": "10m",
                "interrupt_exist_connections": true
            }, {
                "type": "trojan",
                "tag": "old-node",
                "server": "old.example.com",
                "server_port": 443,
                "password": "secret"
            }]
        });

        merge_into_existing_config(
            &mut config,
            vec![json!({
                "type": "trojan",
                "tag": "new-node",
                "server": "new.example.com",
                "server_port": 443,
                "password": "secret",
            })],
            false,
        )
        .expect("merge succeeds");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        let selector = outbounds
            .iter()
            .find(|value| value["tag"] == "select")
            .expect("manual selector");
        let auto = outbounds
            .iter()
            .find(|value| value["tag"] == "auto")
            .expect("auto selector");
        assert_eq!(selector["interrupt_exist_connections"], false);
        assert_eq!(auto["interrupt_exist_connections"], false);
    }

    #[test]
    fn merge_migrates_legacy_remote_dns() {
        let mut config = json!({
            "dns": {
                "servers": [{
                    "type": "tls",
                    "tag": "remote",
                    "server": "8.8.8.8",
                    "server_port": 853,
                    "detour": "manual"
                }]
            },
            "outbounds": []
        });

        merge_into_existing_config(&mut config, Vec::new(), false).expect("merge succeeds");

        let remote = config["dns"]["servers"]
            .as_array()
            .expect("dns servers")
            .iter()
            .find(|server| server["tag"] == "remote")
            .expect("remote DNS server");
        assert_eq!(remote["server"], "223.6.6.6");
        assert!(remote.get("detour").is_none());
    }

    #[test]
    fn merge_preserves_existing_config_and_adds_imported_nodes() {
        let mut config = json!({
            "inbounds": [{
                "type": "mixed",
                "tag": "mixed-in",
                "listen": "127.0.0.1",
                "listen_port": 9000,
            }],
            "outbounds": [{
                "type": "selector",
                "tag": "select",
                "outbounds": ["existing-node"],
                "default": "existing-node",
            }, {
                "type": "trojan",
                "tag": "existing-node",
                "server": "old.example.com",
                "server_port": 443,
                "password": "secret",
            }],
            "route": {
                "final": "existing-node",
            }
        });

        merge_into_existing_config(
            &mut config,
            vec![json!({
                "type": "trojan",
                "tag": "node-a",
                "server": "example.com",
                "server_port": 443,
                "password": "secret",
            })],
            false,
        )
        .expect("merge succeeds");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        let select = outbounds
            .iter()
            .find(|value| value["tag"] == "select")
            .expect("select outbound");
        let members = select["outbounds"].as_array().expect("selector members");
        assert!(members.contains(&Value::String("existing-node".to_string())));
        assert!(members.contains(&Value::String("auto".to_string())));
        assert!(!members.contains(&Value::String("direct".to_string())));
        assert!(members.contains(&Value::String("node-a".to_string())));
        assert_eq!(config["route"]["final"], "existing-node");
        assert!(
            config["route"]["rules"]
                .as_array()
                .expect("route rules")
                .iter()
                .any(|value| value["rule_set"] == "sing-box-tui-bypass"
                    && value["outbound"] == "direct")
        );
        assert!(
            config["route"]["rules"]
                .as_array()
                .expect("route rules")
                .iter()
                .any(|value| value["ip_cidr"] == json!(["100.64.0.0/10"])
                    && value["outbound"] == "direct")
        );
        assert!(
            config["route"]["rule_set"]
                .as_array()
                .expect("rule sets")
                .iter()
                .any(|value| value["tag"] == "sing-box-tui-bypass"
                    && value["path"] == "sing-box-tui-bypass.json")
        );
        assert!(outbounds.iter().any(|value| value["tag"] == "node-a"));
    }

    #[test]
    fn replace_nodes_removes_existing_node_outbounds_but_keeps_special_ones() {
        let mut config = json!({
            "outbounds": [{
                "type": "selector",
                "tag": "select",
                "outbounds": ["old-node"],
                "default": "old-node"
            }, {
                "type": "urltest",
                "tag": "auto",
                "outbounds": ["old-node"],
                "url": "https://www.gstatic.com/generate_204",
                "interval": "10m"
            }, {
                "type": "trojan",
                "tag": "old-node",
                "server": "old.example.com",
                "server_port": 443,
                "password": "secret"
            }, {
                "type": "direct",
                "tag": "direct"
            }],
            "route": {
                "final": "select"
            }
        });

        merge_into_existing_config(
            &mut config,
            vec![json!({
                "type": "vless",
                "tag": "new-node",
                "server": "new.example.com",
                "server_port": 443,
                "uuid": "abc"
            })],
            true,
        )
        .expect("replace succeeds");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert!(!outbounds.iter().any(|value| value["tag"] == "old-node"));
        assert!(outbounds.iter().any(|value| value["tag"] == "new-node"));
        assert!(outbounds.iter().any(|value| value["tag"] == "select"));
        assert!(outbounds.iter().any(|value| value["tag"] == "auto"));
        assert!(outbounds.iter().any(|value| value["tag"] == "direct"));

        let select = outbounds
            .iter()
            .find(|value| value["tag"] == "select")
            .expect("select outbound");
        let members = select["outbounds"].as_array().expect("selector members");
        assert!(!members.contains(&Value::String("old-node".to_string())));
        assert!(members.contains(&Value::String("auto".to_string())));
        assert!(!members.contains(&Value::String("direct".to_string())));
        assert!(members.contains(&Value::String("new-node".to_string())));
    }

    #[test]
    fn provider_node_sets_replace_stale_provider_nodes() {
        let path = temp_config_path("provider-refresh");
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string_pretty(&json!({
                    "outbounds": [{
                        "type": "selector",
                        "tag": "select",
                        "outbounds": ["auto", "direct", "airtcp", "other-provider"]
                    }, {
                        "type": "urltest",
                        "tag": "auto",
                        "outbounds": ["old-airtcp-node", "other-node"]
                    }, {
                        "type": "selector",
                        "tag": "airtcp",
                        "outbounds": ["old-airtcp-node"]
                    }, {
                        "type": "selector",
                        "tag": "other-provider",
                        "outbounds": ["other-node"]
                    }, {
                        "type": "trojan",
                        "tag": "old-airtcp-node",
                        "server": "old.example.com",
                        "server_port": 443,
                        "password": "secret"
                    }, {
                        "type": "trojan",
                        "tag": "other-node",
                        "server": "other.example.com",
                        "server_port": 443,
                        "password": "secret"
                    }]
                }))
                .expect("config serializes")
            ),
        )
        .expect("temp config writes");

        let config = build_full_config_with_provider_node_sets(
            &path,
            vec![ProviderNodeSet {
                provider_name: "airtcp".to_string(),
                nodes: vec![json!({
                    "type": "trojan",
                    "tag": "new-airtcp-node",
                    "server": "new.example.com",
                    "server_port": 443,
                    "password": "secret"
                })],
            }],
            false,
        )
        .expect("provider refresh config builds");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert!(
            !outbounds
                .iter()
                .any(|value| value["tag"] == "old-airtcp-node")
        );
        assert!(
            outbounds
                .iter()
                .any(|value| value["tag"] == "new-airtcp-node")
        );
        assert!(outbounds.iter().any(|value| value["tag"] == "other-node"));

        let provider = outbounds
            .iter()
            .find(|value| value["tag"] == "airtcp")
            .expect("airtcp selector");
        assert_eq!(
            provider["outbounds"],
            Value::Array(vec![Value::String("new-airtcp-node".to_string())])
        );

        let selector = outbounds
            .iter()
            .find(|value| value["tag"] == "select")
            .expect("root selector");
        let members = selector["outbounds"].as_array().expect("selector members");
        assert!(members.contains(&Value::String("airtcp".to_string())));
        assert!(members.contains(&Value::String("other-node".to_string())));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn empty_provider_node_set_removes_stale_provider_selector() {
        let path = temp_config_path("empty-provider-refresh");
        fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string_pretty(&json!({
                    "outbounds": [{
                        "type": "selector",
                        "tag": "select",
                        "outbounds": ["auto", "direct", "airtcp"]
                    }, {
                        "type": "urltest",
                        "tag": "auto",
                        "outbounds": ["old-airtcp-node"]
                    }, {
                        "type": "selector",
                        "tag": "airtcp",
                        "outbounds": ["old-airtcp-node"]
                    }, {
                        "type": "trojan",
                        "tag": "old-airtcp-node",
                        "server": "old.example.com",
                        "server_port": 443,
                        "password": "secret"
                    }]
                }))
                .expect("config serializes")
            ),
        )
        .expect("temp config writes");

        let config = build_full_config_with_provider_node_sets(
            &path,
            vec![ProviderNodeSet {
                provider_name: "airtcp".to_string(),
                nodes: Vec::new(),
            }],
            false,
        )
        .expect("provider refresh config builds");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert!(!outbounds.iter().any(|value| value["tag"] == "airtcp"));
        assert!(
            !outbounds
                .iter()
                .any(|value| value["tag"] == "old-airtcp-node")
        );

        let selector = outbounds
            .iter()
            .find(|value| value["tag"] == "select")
            .expect("root selector");
        let members = selector["outbounds"].as_array().expect("selector members");
        assert!(!members.contains(&Value::String("airtcp".to_string())));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn merge_migrates_legacy_inbound_fields_to_route_actions() {
        let mut config = json!({
            "inbounds": [{
                "type": "mixed",
                "listen": "127.0.0.1",
                "listen_port": 6780,
                "sniff": true,
                "sniff_timeout": "1s",
                "domain_strategy": "ipv4_only"
            }],
            "outbounds": []
        });

        merge_into_existing_config(&mut config, Vec::new(), false).expect("merge succeeds");

        assert!(config["inbounds"][0].get("sniff").is_none());
        assert!(config["inbounds"][0].get("domain_strategy").is_none());
        let rules = config["route"]["rules"].as_array().expect("route rules");
        assert!(rules.contains(&json!({
            "action": "resolve",
            "strategy": "ipv4_only"
        })));
        assert!(rules.contains(&json!({
            "action": "sniff",
            "timeout": "1s"
        })));
    }

    #[test]
    fn default_tun_inbound_is_a_tun_inbound_with_auto_route() {
        let inbound = default_tun_inbound();
        assert_eq!(inbound["type"], "tun");
        assert_eq!(inbound["tag"], "tun-in");
        assert_eq!(inbound["address"], json!(["172.19.0.1/30"]));
        assert_eq!(inbound["auto_route"], true);
        assert_eq!(inbound["strict_route"], true);
        assert_eq!(inbound["stack"], "mixed");
    }

    #[test]
    fn internet_tun_update_waits_for_the_shared_config_mutation_lock() {
        let path = temp_config_path("internet-tun-mutation-lock");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "inbounds": [{ "type": "mixed", "listen_port": 6780 }],
                "outbounds": [{ "type": "direct", "tag": "direct" }]
            }))
            .expect("serializes config"),
        )
        .expect("writes config");

        let guard = lock_config_mutation();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker_path = path.clone();
        let worker = thread::spawn(move || {
            started_tx.send(()).expect("signals worker start");
            let result = set_internet_tun_mode(&worker_path, true, None);
            finished_tx.send(result).expect("reports worker result");
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker starts");
        assert!(
            matches!(
                finished_rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout)
            ),
            "the TUN read-modify-write must wait while another config mutation is active"
        );
        drop(guard);

        let update = finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker resumes after releasing the mutation lock")
            .expect("TUN update succeeds");
        assert!(update.changed);
        worker.join().expect("worker exits cleanly");
        let updated: Value = serde_json::from_str(
            &fs::read_to_string(&path).expect("reads config after serialized update"),
        )
        .expect("parses config after serialized update");
        assert_eq!(updated["route"]["auto_detect_interface"], true);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_internet_tun_mode_adds_and_removes_tun_inbound_idempotently() {
        let path = temp_config_path("internet-tun-toggle");
        let base = json!({
            "inbounds": [{
                "type": "mixed",
                "listen": "::",
                "listen_port": 6780,
                "set_system_proxy": false
            }],
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "auto_detect_interface": false }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        assert!(!config_has_internet_tun_inbound(&path).expect("detects"));

        let enabled_update = set_internet_tun_mode(&path, true, None).expect("enables");
        assert!(enabled_update.changed);
        assert_eq!(
            enabled_update.auto_detect_interface_before_enable,
            RouteAutoDetectInterfaceState::Disabled
        );
        assert!(config_has_internet_tun_inbound(&path).expect("detects after enable"));
        let enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        assert_eq!(
            enabled["inbounds"]
                .as_array()
                .expect("inbounds array")
                .iter()
                .filter(|value| value["type"] == "tun")
                .count(),
            1
        );
        assert_eq!(enabled["route"]["auto_detect_interface"], true);

        // Enabling again is a no-op and keeps the mixed inbound.
        assert!(
            !set_internet_tun_mode(&path, true, None)
                .expect("enables idempotently")
                .changed
        );
        let again: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        assert!(
            again["inbounds"]
                .as_array()
                .expect("inbounds array")
                .iter()
                .any(|value| value["type"] == "mixed")
        );

        assert!(
            set_internet_tun_mode(
                &path,
                false,
                Some(enabled_update.auto_detect_interface_before_enable),
            )
            .expect("disables")
            .changed
        );
        assert!(!config_has_internet_tun_inbound(&path).expect("detects after disable"));
        let disabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        assert_eq!(disabled["route"]["auto_detect_interface"], false);
        assert!(
            !set_internet_tun_mode(&path, false, None)
                .expect("disables idempotently")
                .changed
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_internet_tun_mode_restores_the_previous_route_interface_state() {
        for (name, route) in [
            ("route-missing", None),
            ("field-missing", Some(json!({ "rules": [] }))),
            (
                "already-enabled",
                Some(json!({ "auto_detect_interface": true })),
            ),
        ] {
            let path = temp_config_path(name);
            let mut base = json!({
                "inbounds": [{ "type": "mixed", "listen_port": 6780 }],
                "outbounds": [{ "type": "direct", "tag": "direct" }]
            });
            if let Some(route) = route {
                base["route"] = route;
            }
            let expected_route = base.get("route").cloned();
            fs::write(
                &path,
                serde_json::to_string_pretty(&base).expect("serializes"),
            )
            .expect("writes config");

            let enabled = set_internet_tun_mode(&path, true, None).expect("enables");
            assert!(enabled.changed);
            let disabled = set_internet_tun_mode(
                &path,
                false,
                Some(enabled.auto_detect_interface_before_enable),
            )
            .expect("disables");
            assert!(disabled.changed);

            let restored: Value =
                serde_json::from_str(&fs::read_to_string(&path).expect("reads restored config"))
                    .expect("parses restored config");
            assert_eq!(restored.get("route"), expected_route.as_ref(), "{name}");
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn internet_tun_toggle_preserves_custom_tun_inbounds() {
        let path = temp_config_path("custom-tun");
        let base = json!({
            "inbounds": [{
                "type": "tun",
                "tag": "custom-tun",
                "address": ["172.20.0.1/30"],
                "auto_route": false
            }],
            "route": { "auto_detect_interface": true }
        });
        let original = serde_json::to_string_pretty(&base).expect("serializes");
        fs::write(&path, &original).expect("writes config");

        assert!(!config_has_internet_tun_inbound(&path).expect("detects managed TUN"));
        let error = set_internet_tun_mode(&path, true, None)
            .expect_err("a second managed TUN must not be added over a custom TUN");
        assert!(error.to_string().contains("another TUN inbound"));
        assert_eq!(fs::read_to_string(&path).expect("reads config"), original);
        assert!(
            !set_internet_tun_mode(&path, false, None)
                .expect("disabling managed TUN is a no-op")
                .changed
        );
        let preserved: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads preserved config"))
                .expect("parses preserved config");
        assert_eq!(preserved["inbounds"][0]["tag"], "custom-tun");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enabling_rejects_a_non_tun_inbound_using_the_reserved_tag() {
        let path = temp_config_path("reserved-tun-tag");
        let config = json!({
            "inbounds": [{
                "type": "mixed",
                "tag": "tun-in",
                "listen": "::",
                "listen_port": 6780
            }],
            "outbounds": [{ "type": "direct", "tag": "direct" }]
        });
        let original = serde_json::to_string_pretty(&config).expect("serializes");
        fs::write(&path, &original).expect("writes config");

        let inspected = inspect_tun_config(&path).expect("inspects reserved tag conflict");
        assert!(!inspected.managed_internet_tun);
        assert!(inspected.reserved_tag_conflict);
        let error = set_internet_tun_mode(&path, true, None)
            .expect_err("reserved tag collision must be rejected");
        assert!(error.to_string().contains("tag 'tun-in'"));
        assert_eq!(fs::read_to_string(&path).expect("reads config"), original);
        assert!(
            !set_internet_tun_mode(&path, false, None)
                .expect("disabling an absent managed TUN is safe")
                .changed
        );
        assert_eq!(fs::read_to_string(&path).expect("reads config"), original);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enabling_rejects_duplicate_managed_tun_tags_while_disable_can_recover() {
        let path = temp_config_path("duplicate-managed-tun-tag");
        let config = json!({
            "inbounds": [default_tun_inbound(), default_tun_inbound()],
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "auto_detect_interface": true }
        });
        let original = serde_json::to_string_pretty(&config).expect("serializes");
        fs::write(&path, &original).expect("writes config");

        let inspected = inspect_tun_config(&path).expect("inspects duplicate managed tags");
        assert!(inspected.managed_internet_tun);
        assert!(inspected.reserved_tag_conflict);
        let error = set_internet_tun_mode(&path, true, None)
            .expect_err("duplicate managed tags must be rejected");
        assert!(error.to_string().contains("tag 'tun-in'"));
        assert_eq!(fs::read_to_string(&path).expect("reads config"), original);

        let disabled =
            set_internet_tun_mode(&path, false, Some(RouteAutoDetectInterfaceState::Disabled))
                .expect("disable removes duplicate managed TUN inbounds");
        assert!(disabled.changed);
        let recovered = inspect_tun_config(&path).expect("inspects recovered config");
        assert!(!recovered.managed_internet_tun);
        assert!(!recovered.reserved_tag_conflict);
        assert_eq!(
            recovered.auto_detect_interface,
            RouteAutoDetectInterfaceState::Disabled
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn enabling_rejects_existing_managed_and_custom_tuns() {
        let path = temp_config_path("managed-and-custom-tun");
        let config = json!({
            "inbounds": [
                default_tun_inbound(),
                {
                    "type": "tun",
                    "tag": "custom-tun",
                    "address": ["172.20.0.1/30"],
                    "auto_route": false
                }
            ],
            "route": { "auto_detect_interface": true }
        });
        let original = serde_json::to_string_pretty(&config).expect("serializes");
        fs::write(&path, &original).expect("writes config");

        let error =
            set_internet_tun_mode(&path, true, None).expect_err("managed and custom TUNs conflict");
        assert!(error.to_string().contains("another TUN inbound"));
        assert_eq!(fs::read_to_string(&path).expect("reads config"), original);

        assert!(
            set_internet_tun_mode(&path, false, Some(RouteAutoDetectInterfaceState::Disabled),)
                .expect("managed TUN can be removed from a conflicting config")
                .changed
        );
        let recovered: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads recovered config"))
                .expect("parses recovered config");
        assert_eq!(recovered["inbounds"].as_array().map(Vec::len), Some(1));
        assert_eq!(recovered["inbounds"][0]["tag"], "custom-tun");
        assert_eq!(recovered["route"]["auto_detect_interface"], true);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn disabling_absent_internet_tun_does_not_create_inbounds() {
        let path = temp_config_path("absent-tun-without-inbounds");
        let original = r#"{"route":{"auto_detect_interface":true}}"#;
        fs::write(&path, original).expect("writes config");

        assert!(
            !set_internet_tun_mode(&path, false, None)
                .expect("disabling absent managed TUN is a no-op")
                .changed
        );
        assert_eq!(fs::read_to_string(&path).expect("reads config"), original);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_china_ip_routing_adds_and_removes_rules_idempotently() {
        let path = temp_config_path("china-ip-routing-toggle");
        let base = json!({
            "inbounds": [{
                "type": "mixed",
                "listen": "::",
                "listen_port": 6780,
                "set_system_proxy": false
            }],
            "outbounds": [
                { "type": "direct", "tag": "国内直连" },
                { "type": "selector", "tag": "手动选择", "outbounds": ["国内直连"] }
            ],
            "dns": {
                "servers": [
                    { "type": "tls", "tag": "remote", "server": "8.8.8.8" },
                    { "type": "tls", "tag": "local", "server": "223.5.5.5" }
                ],
                "rules": [
                    { "clash_mode": "全局", "server": "remote" },
                    { "clash_mode": "直连", "server": "local" },
                    { "clash_mode": "规则", "server": "remote" }
                ]
            },
            "route": {
                "rules": [
                    { "action": "hijack-dns" },
                    { "clash_mode": "直连", "outbound": "国内直连" },
                    { "clash_mode": "全局", "outbound": "手动选择" },
                    { "ip_is_private": true, "outbound": "国内直连" },
                    { "clash_mode": "规则", "outbound": "手动选择" }
                ],
                "rule_set": []
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        assert!(!config_has_china_ip_routing(&path).expect("detects"));

        assert!(set_china_ip_routing(&path, true).expect("enables"));
        assert!(config_has_china_ip_routing(&path).expect("detects after enable"));
        let enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        let rule_set_tags = enabled["route"]["rule_set"]
            .as_array()
            .expect("rule sets")
            .iter()
            .filter_map(|value| value["tag"].as_str())
            .collect::<Vec<_>>();
        for tag in [
            "geoip-cn",
            "geosite-cn",
            "geosite-geolocation-cn",
            "geosite-geolocation-!cn",
        ] {
            assert!(rule_set_tags.contains(&tag), "missing rule-set {tag}");
        }
        assert!(!rule_set_tags.contains(&"AdGuardSDNSFilter"));
        // Rule-sets are local (pre-downloaded) binary files, never remote, so startup does not
        // depend on reaching the download URL.
        let rule_sets = enabled["route"]["rule_set"].as_array().expect("rule sets");
        for rule_set in rule_sets {
            if rule_set_tags.contains(&rule_set["tag"].as_str().unwrap_or_default()) {
                assert_eq!(rule_set["type"], "local");
                assert_eq!(rule_set["format"], "binary");
                assert!(rule_set.get("path").is_some());
                assert!(rule_set.get("url").is_none());
            }
        }
        // China route rule is inserted before the 规则 catch-all, not after it.
        let route_rules = enabled["route"]["rules"].as_array().expect("route rules");
        let final_rule = route_rules.last().expect("route rules");
        assert_eq!(final_rule["clash_mode"], "规则");

        // Enabling again is a no-op.
        assert!(!set_china_ip_routing(&path, true).expect("enables idempotently"));

        assert!(set_china_ip_routing(&path, false).expect("disables"));
        assert!(!config_has_china_ip_routing(&path).expect("detects after disable"));
        assert!(!set_china_ip_routing(&path, false).expect("disables idempotently"));
        let disabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        let rule_set_tags = disabled["route"]["rule_set"]
            .as_array()
            .expect("rule sets")
            .iter()
            .filter_map(|value| value["tag"].as_str())
            .collect::<Vec<_>>();
        assert!(!rule_set_tags.contains(&"geoip-cn"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_china_ip_routing_rejects_when_no_direct_outbound_exists() {
        let path = temp_config_path("china-ip-routing-no-direct");
        let base = json!({
            "outbounds": [{ "type": "selector", "tag": "手动选择", "outbounds": [] }],
            "route": { "rules": [], "rule_set": [] }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        let error = set_china_ip_routing(&path, true).expect_err("rejects without direct");
        assert!(error.to_string().contains("direct outbound"));

        let after: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        assert_eq!(after, base, "config must be left unchanged on rejection");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_china_ip_routing_uses_custom_direct_outbound_tag() {
        let path = temp_config_path("china-ip-routing-custom-direct");
        let base = json!({
            "outbounds": [{ "type": "direct", "tag": "bypass" }],
            "route": {
                "rules": [{ "clash_mode": "规则", "outbound": "bypass" }],
                "rule_set": []
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        set_china_ip_routing(&path, true).expect("enables");
        let enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        let geoip_rule = enabled["route"]["rules"]
            .as_array()
            .expect("route rules")
            .iter()
            .find(|rule| rule["rule_set"] == "geoip-cn")
            .expect("geoip route rule");
        assert_eq!(geoip_rule["outbound"], "bypass");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_china_ip_routing_adds_missing_dns_servers() {
        let path = temp_config_path("china-ip-routing-dns-servers");
        let base = json!({
            "outbounds": [
                { "type": "direct", "tag": "国内直连" },
                { "type": "selector", "tag": "手动选择", "outbounds": ["国内直连"] }
            ],
            "route": {
                "rules": [{ "clash_mode": "规则", "outbound": "手动选择" }],
                "rule_set": []
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        set_china_ip_routing(&path, true).expect("enables");
        let enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        let dns_tags = enabled["dns"]["servers"]
            .as_array()
            .expect("dns servers")
            .iter()
            .filter_map(|server| server["tag"].as_str())
            .collect::<Vec<_>>();
        assert!(dns_tags.contains(&"local"));
        assert!(dns_tags.contains(&"remote"));
        let remote = enabled["dns"]["servers"]
            .as_array()
            .expect("dns servers")
            .iter()
            .find(|server| server["tag"] == "remote")
            .expect("remote DNS server");
        assert_eq!(remote["server"], "223.6.6.6");
        assert!(remote.get("detour").is_none());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_china_ip_routing_migrates_legacy_remote_dns() {
        let path = temp_config_path("china-ip-routing-direct-remote-dns");
        let base = json!({
            "dns": {
                "servers": [{
                    "type": "tls",
                    "tag": "remote",
                    "server": "8.8.8.8",
                    "server_port": 853,
                    "detour": "手动选择"
                }]
            },
            "outbounds": [
                { "type": "direct", "tag": "国内直连" },
                { "type": "selector", "tag": "手动选择", "outbounds": ["国内直连"] }
            ],
            "route": {
                "rules": [{ "clash_mode": "规则", "outbound": "手动选择" }],
                "rule_set": []
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        set_china_ip_routing(&path, true).expect("enables");
        let enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        let remote = enabled["dns"]["servers"]
            .as_array()
            .expect("dns servers")
            .iter()
            .find(|server| server["tag"] == "remote")
            .expect("remote DNS server");
        assert_eq!(remote["server"], "223.6.6.6");
        assert!(remote.get("detour").is_none());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_china_ip_routing_preserves_custom_remote_dns_detour() {
        let path = temp_config_path("china-ip-routing-custom-remote-dns");
        let base = json!({
            "dns": {
                "servers": [{
                    "type": "tls",
                    "tag": "remote",
                    "server": "1.1.1.1",
                    "server_port": 853,
                    "detour": "手动选择"
                }]
            },
            "outbounds": [
                { "type": "direct", "tag": "国内直连" },
                { "type": "selector", "tag": "手动选择", "outbounds": ["国内直连"] }
            ],
            "route": {
                "rules": [{ "clash_mode": "规则", "outbound": "手动选择" }],
                "rule_set": []
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        set_china_ip_routing(&path, true).expect("enables");
        let enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        let remote = enabled["dns"]["servers"]
            .as_array()
            .expect("dns servers")
            .iter()
            .find(|server| server["tag"] == "remote")
            .expect("remote DNS server");
        assert_eq!(remote["detour"], "手动选择");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_china_ip_routing_preserves_unrelated_rule_set_members() {
        let path = temp_config_path("china-ip-routing-compound-rule");
        let base = json!({
            "outbounds": [
                { "type": "direct", "tag": "国内直连" },
                { "type": "selector", "tag": "手动选择", "outbounds": ["国内直连"] }
            ],
            "route": {
                "rules": [
                    { "rule_set": ["geosite-cn", "custom-corporate"], "outbound": "国内直连" },
                    { "clash_mode": "规则", "outbound": "手动选择" }
                ],
                "rule_set": []
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        set_china_ip_routing(&path, true).expect("enables");
        set_china_ip_routing(&path, false).expect("disables");
        let disabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        let rules = disabled["route"]["rules"].as_array().expect("route rules");
        let compound = rules
            .iter()
            .find(|rule| rule["rule_set"].is_array())
            .expect("compound rule survives");
        assert_eq!(compound["rule_set"], json!(["custom-corporate"]));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_china_ip_routing_keeps_english_global_mode_ahead_of_china_rules() {
        let path = temp_config_path("china-ip-routing-english-modes");
        let base = json!({
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "selector", "tag": "select", "outbounds": ["direct"] }
            ],
            "route": {
                "rules": [
                    { "clash_mode": "Direct", "outbound": "direct" },
                    { "clash_mode": "Global", "outbound": "select" },
                    { "clash_mode": "Rule", "outbound": "select" }
                ],
                "rule_set": []
            }
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");

        set_china_ip_routing(&path, true).expect("enables");
        let enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        let rules = enabled["route"]["rules"].as_array().expect("route rules");
        let geoip_index = rules
            .iter()
            .position(|rule| rule["rule_set"] == "geoip-cn")
            .expect("geoip rule present");
        let global_index = rules
            .iter()
            .position(|rule| rule["clash_mode"] == "Global")
            .expect("global rule present");
        assert!(
            geoip_index > global_index,
            "China rules must come after the global-mode override"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_tailscale_config_manages_endpoint_dns_and_routes_idempotently() {
        let path = temp_config_path("tailscale");
        let base = json!({
            "dns": {"servers": [], "rules": [{"server": "remote"}]},
            "endpoints": [{"type": "wireguard", "tag": "other"}],
            "route": {"rules": [{"action": "hijack-dns"}, {"outbound": "direct"}]}
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");
        let options = TailscaleConfigOptions {
            tailnet_domain: ".example.ts.net".to_string(),
            hostname: Some("laptop-sing-box".to_string()),
        };

        assert!(set_tailscale_config(&path, Some(options.clone())).expect("enables"));
        assert!(!set_tailscale_config(&path, Some(options)).expect("second enable is stable"));
        let state = inspect_tailscale_config(&path).expect("inspects");
        assert_eq!(
            state,
            TailscaleConfigState {
                enabled: true,
                tailnet_domain: Some("example.ts.net".to_string()),
                hostname: Some("laptop-sing-box".to_string()),
            }
        );
        let enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        assert!(value_references(&enabled, TAILSCALE_ENDPOINT_TAG));
        assert!(value_references(&enabled, TAILSCALE_DNS_TAG));
        assert!(value_references(&enabled, TAILSCALE_IPV6_CIDR));
        assert!(enabled["dns"]["rules"].as_array().is_some_and(|rules| {
            rules
                .iter()
                .any(is_managed_tailscale_control_plane_dns_rule)
        }));

        assert!(set_tailscale_config(&path, None).expect("disables"));
        let disabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        assert!(!value_references(&disabled, TAILSCALE_ENDPOINT_TAG));
        assert!(!value_references(&disabled, TAILSCALE_DNS_TAG));
        assert_eq!(disabled["endpoints"][0]["tag"], "other");
        assert_eq!(disabled["route"]["rules"][0]["action"], "hijack-dns");
        assert_eq!(disabled["route"]["rules"][1]["outbound"], "direct");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn set_tailscale_config_rejects_an_unmanaged_dns_tag_collision() {
        let path = temp_config_path("tailscale-dns-collision");
        let base = json!({
            "dns": {
                "servers": [{"type": "local", "tag": TAILSCALE_DNS_TAG}],
                "rules": [{"domain_suffix": ["corp.example"], "server": TAILSCALE_DNS_TAG}]
            },
            "route": {"rules": []}
        });
        let original = serde_json::to_string_pretty(&base).expect("serializes");
        fs::write(&path, &original).expect("writes config");

        let error = set_tailscale_config(
            &path,
            Some(TailscaleConfigOptions {
                tailnet_domain: "example.ts.net".to_string(),
                hostname: None,
            }),
        )
        .expect_err("unmanaged DNS tag must be rejected");
        assert!(error.to_string().contains("tailscale-dns"));
        assert_eq!(fs::read_to_string(&path).expect("reads"), original);
        assert!(!set_tailscale_config(&path, None).expect("disable is a no-op"));
        assert_eq!(fs::read_to_string(&path).expect("reads"), original);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn updating_tailscale_config_preserves_custom_routes_through_the_endpoint() {
        let path = temp_config_path("tailscale-custom-route");
        let base = json!({
            "dns": {"servers": [], "rules": []},
            "route": {"rules": [{"action": "hijack-dns"}]}
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&base).expect("serializes"),
        )
        .expect("writes config");
        set_tailscale_config(
            &path,
            Some(TailscaleConfigOptions {
                tailnet_domain: "old.ts.net".to_string(),
                hostname: None,
            }),
        )
        .expect("enables");
        let mut enabled: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        enabled["route"]["rules"]
            .as_array_mut()
            .expect("route rules")
            .push(json!({
                "domain_suffix": ["custom.internal", "other.internal"],
                "action": "route",
                "outbound": TAILSCALE_ENDPOINT_TAG
            }));
        fs::write(
            &path,
            serde_json::to_string_pretty(&enabled).expect("serializes"),
        )
        .expect("writes config");

        set_tailscale_config(
            &path,
            Some(TailscaleConfigOptions {
                tailnet_domain: "new.ts.net".to_string(),
                hostname: Some("new-host".to_string()),
            }),
        )
        .expect("updates");
        let updated: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reads")).expect("parses");
        assert!(updated["route"]["rules"].as_array().is_some_and(|rules| {
            rules.iter().any(|rule| {
                rule.get("domain_suffix") == Some(&json!(["custom.internal", "other.internal"]))
                    && rule.get("outbound").and_then(Value::as_str) == Some(TAILSCALE_ENDPOINT_TAG)
            })
        }));

        let _ = fs::remove_file(path);
    }

    fn value_references(value: &Value, needle: &str) -> bool {
        match value {
            Value::String(value) => value.contains(needle),
            Value::Array(values) => values.iter().any(|value| value_references(value, needle)),
            Value::Object(values) => values.values().any(|value| value_references(value, needle)),
            _ => false,
        }
    }

    fn temp_config_path(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-{label}-{nanos}.json"))
    }
}
