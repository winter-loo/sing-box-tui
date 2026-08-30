use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::atomic_file::write_atomic;
use crate::automatic_selection::NodeViewId;
use crate::config::RouteAutoDetectInterfaceState;
use crate::defaults::DEFAULT_BYPASS_RULE_SET_PATH;
use crate::node_quality_path::{canonical_config_target, canonical_file_target};

const DEFAULT_TUI_STATE_PATH: &str = "sing-box-tui.json";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrivateAccessProfileState {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) manifest_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) password_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bridge_listen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tun_helper: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) tls_verify: bool,
    #[serde(default)]
    pub(crate) use_internet_proxy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) background_pid: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TuiRuntimeState {
    #[serde(default)]
    pub(crate) benchmark_filter: String,
    #[serde(default)]
    pub(crate) auto_pick_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_pick_selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active_node_view: Option<NodeViewId>,
    #[serde(default)]
    pub(crate) current_selected_nodes: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) bypass_entries: Vec<String>,
    #[serde(default)]
    pub(crate) onboarding_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) benchmark_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sustained_target_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) benchmark_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) benchmark_request_timeout: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) benchmark_max_concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verify_targets: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_select_threshold_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_select_interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) system_proxy_server: Option<String>,
    #[serde(default)]
    pub(crate) system_proxy_server_override: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) system_proxy_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) china_ip_routing_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tailscale_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tailscale_tailnet_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tailscale_hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tun_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tun_auto_detect_interface_before_enable: Option<RouteAutoDetectInterfaceState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) private_access_profiles: Vec<PrivateAccessProfileState>,
}

#[derive(Clone, Debug)]
pub(crate) struct TuiStateStore {
    path: PathBuf,
}

impl TuiStateStore {
    pub(crate) fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub(crate) fn exists(&self) -> bool {
        self.path.exists()
    }

    pub(crate) fn load(&self) -> Result<TuiRuntimeState> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(TuiRuntimeState::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", self.path.display()))
    }

    pub(crate) fn save(&self, state: &TuiRuntimeState) -> Result<()> {
        let text = serde_json::to_string_pretty(state).context("failed to encode TUI state")?;
        write_atomic(&self.path, format!("{text}\n").as_bytes())
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BypassRuleSetStore {
    path: PathBuf,
}

impl BypassRuleSetStore {
    pub(crate) fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub(crate) fn save(&self, entries: &[String]) -> Result<()> {
        let value = bypass_rule_set_value(entries);
        let text =
            serde_json::to_string_pretty(&value).context("failed to encode bypass rule-set")?;
        fs::write(&self.path, format!("{text}\n"))
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}

pub(crate) fn default_tui_state_path() -> PathBuf {
    env::var("SING_BOX_TUI_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TUI_STATE_PATH))
}

pub(crate) fn resolved_tui_bypass_rule_set_path(config_path: &Path) -> Result<PathBuf> {
    resolve_tui_bypass_rule_set_path(
        config_path,
        env::var_os("SING_BOX_TUI_BYPASS_RULE_SET")
            .as_deref()
            .map(Path::new),
    )
}

fn resolve_tui_bypass_rule_set_path(
    config_path: &Path,
    configured_path: Option<&Path>,
) -> Result<PathBuf> {
    let config_target = canonical_config_target(config_path)?;
    let config_parent = config_target
        .parent()
        .context("active config target must have a parent directory")?;
    let candidate = match configured_path {
        Some(path) if path.as_os_str().is_empty() => {
            bail!("SING_BOX_TUI_BYPASS_RULE_SET must not be empty")
        }
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => config_parent.join(path),
        None => config_parent.join(DEFAULT_BYPASS_RULE_SET_PATH),
    };
    canonical_file_target(&candidate, "TUI bypass rule-set")
}

pub(crate) fn parse_bypass_entries(input: &str) -> Vec<String> {
    let mut entries = Vec::new();
    for item in input.split([',', '，', '\n', '\r', '\t', ' ']) {
        let Some(entry) = normalize_bypass_entry(item) else {
            continue;
        };
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries
}

fn bypass_rule_set_value(entries: &[String]) -> Value {
    let mut domains = Vec::new();
    let mut ip_cidrs = Vec::new();
    for entry in entries {
        if is_ip_entry(entry) {
            ip_cidrs.push(entry.clone());
        } else {
            domains.push(normalize_domain_suffix(entry));
        }
    }

    let mut rules = Vec::new();
    if !domains.is_empty() {
        rules.push(json!({ "domain_suffix": domains }));
    }
    if !ip_cidrs.is_empty() {
        rules.push(json!({ "ip_cidr": ip_cidrs }));
    }
    json!({
        "version": 1,
        "rules": rules,
    })
}

fn normalize_bypass_entry(value: &str) -> Option<String> {
    let mut value = value.trim();
    if value.is_empty() {
        return None;
    }
    let mut is_url = false;
    if let Some(rest) = value.strip_prefix("http://") {
        value = rest;
        is_url = true;
    } else if let Some(rest) = value.strip_prefix("https://") {
        value = rest;
        is_url = true;
    }
    if is_url {
        value = value.split('/').next().unwrap_or(value);
    }
    value = value.trim_start_matches("*.");
    Some(value.trim_start_matches('.').to_ascii_lowercase())
}

fn normalize_domain_suffix(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .to_ascii_lowercase()
}

fn is_ip_entry(value: &str) -> bool {
    value.contains('/') || IpAddr::from_str(value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{PrivateAccessProfileState, TuiRuntimeState, resolve_tui_bypass_rule_set_path};
    use crate::config::RouteAutoDetectInterfaceState;
    use crate::defaults::DEFAULT_BYPASS_RULE_SET_PATH;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn bypass_rule_set_paths_are_bound_to_the_canonical_config_directory() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sing-box-tui-bypass-path-{nonce}"));
        fs::create_dir_all(&root).expect("create config directory");
        let root = root.canonicalize().expect("canonicalize config directory");
        let config = root.join("config.json");

        assert_eq!(
            resolve_tui_bypass_rule_set_path(&config, None).expect("resolve default path"),
            root.join(DEFAULT_BYPASS_RULE_SET_PATH)
        );
        assert_eq!(
            resolve_tui_bypass_rule_set_path(&config, Some(Path::new("nested/custom.json")))
                .expect("resolve relative override"),
            root.join("nested/custom.json")
        );
        let absolute = root.join("absolute.json");
        assert_eq!(
            resolve_tui_bypass_rule_set_path(&config, Some(&absolute))
                .expect("resolve absolute override"),
            absolute
        );
        assert!(resolve_tui_bypass_rule_set_path(&config, Some(Path::new(""))).is_err());
        assert!(!root.join("nested").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn default_bypass_path_follows_a_cross_directory_config_symlink() {
        use std::os::unix::fs::symlink;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("sing-box-tui-bypass-alias-{nonce}"));
        let real = root.join("real");
        let alias = root.join("alias");
        fs::create_dir_all(&real).expect("create real directory");
        fs::create_dir_all(&alias).expect("create alias directory");
        fs::write(real.join("config.json"), b"{}\n").expect("write real config");
        symlink("../real/config.json", alias.join("config.json"))
            .expect("create cross-directory config alias");
        let real = real.canonicalize().expect("canonicalize real directory");

        assert_eq!(
            resolve_tui_bypass_rule_set_path(&alias.join("config.json"), None)
                .expect("resolve default through config alias"),
            real.join(DEFAULT_BYPASS_RULE_SET_PATH)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn state_without_private_access_profiles_uses_empty_profile_list() {
        let state: TuiRuntimeState = serde_json::from_str(
            r#"{
              "benchmark_filter": "",
              "auto_pick_enabled": true,
              "current_selected_nodes": {},
              "bypass_entries": ["tianditu.gov.cn"],
              "onboarding_complete": true,
              "benchmark_url": "https://www.gstatic.com/generate_204",
              "benchmark_timeout_ms": 5000,
              "benchmark_request_timeout": 12.0,
              "benchmark_max_concurrency": 16,
              "system_proxy_server": "127.0.0.1:6780",
              "system_proxy_server_override": false
            }"#,
        )
        .expect("state parses");

        assert!(state.private_access_profiles.is_empty());
    }

    #[test]
    fn save_shape_omits_empty_private_access_service_options() {
        let state = TuiRuntimeState {
            private_access_profiles: vec![PrivateAccessProfileState {
                id: "hillstone".to_string(),
                password_env: Some("HILLSTONE_PASSWORD".to_string()),
                ..PrivateAccessProfileState::default()
            }],
            ..TuiRuntimeState::default()
        };

        let value = serde_json::to_value(&state).expect("serializes");
        let profile = &value["private_access_profiles"][0];

        assert_eq!(profile["id"], "hillstone");
        assert_eq!(profile["password_env"], "HILLSTONE_PASSWORD");
        assert!(profile.get("server").is_none());
        assert!(profile.get("username").is_none());
        assert!(profile.get("password").is_none());
    }

    #[test]
    fn sonicwall_profile_persists_internet_proxy_choice() {
        let state = TuiRuntimeState {
            private_access_profiles: vec![PrivateAccessProfileState {
                id: "sonicwall".to_string(),
                use_internet_proxy: true,
                ..PrivateAccessProfileState::default()
            }],
            ..TuiRuntimeState::default()
        };

        let json = serde_json::to_string(&state).expect("serializes");
        let restored: TuiRuntimeState = serde_json::from_str(&json).expect("parses");

        assert!(restored.private_access_profiles[0].use_internet_proxy);
    }

    #[test]
    fn china_ip_routing_enabled_is_omitted_when_unset_and_persists_when_set() {
        let state = TuiRuntimeState::default();
        let value = serde_json::to_value(&state).expect("serializes");
        assert!(value.get("china_ip_routing_enabled").is_none());

        let state = TuiRuntimeState {
            china_ip_routing_enabled: Some(true),
            ..TuiRuntimeState::default()
        };
        let json = serde_json::to_string(&state).expect("serializes");
        let restored: TuiRuntimeState = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored.china_ip_routing_enabled, Some(true));
    }

    #[test]
    fn tailscale_settings_round_trip() {
        let state = TuiRuntimeState {
            tailscale_enabled: Some(true),
            tailscale_tailnet_domain: Some("example.ts.net".to_string()),
            tailscale_hostname: Some("laptop-sing-box".to_string()),
            ..TuiRuntimeState::default()
        };
        let json = serde_json::to_string(&state).expect("serializes");
        let restored: TuiRuntimeState = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored.tailscale_enabled, Some(true));
        assert_eq!(
            restored.tailscale_tailnet_domain.as_deref(),
            Some("example.ts.net")
        );
        assert_eq!(
            restored.tailscale_hostname.as_deref(),
            Some("laptop-sing-box")
        );
    }

    #[test]
    fn tun_enabled_is_omitted_when_not_explicitly_set() {
        let state = TuiRuntimeState::default();
        let value = serde_json::to_value(&state).expect("serializes");
        assert!(value.get("tun_enabled").is_none());
    }

    #[test]
    fn system_proxy_intent_round_trips_independently_from_exit_cleanup() {
        let state = TuiRuntimeState {
            system_proxy_enabled: Some(true),
            ..TuiRuntimeState::default()
        };
        let json = serde_json::to_string(&state).expect("serializes");
        let restored: TuiRuntimeState = serde_json::from_str(&json).expect("parses");

        assert_eq!(restored.system_proxy_enabled, Some(true));
    }

    #[test]
    fn tun_enabled_persists_when_explicitly_set() {
        let state = TuiRuntimeState {
            tun_enabled: Some(true),
            tun_auto_detect_interface_before_enable: Some(
                RouteAutoDetectInterfaceState::FieldMissing,
            ),
            ..TuiRuntimeState::default()
        };
        let json = serde_json::to_string(&state).expect("serializes");
        let restored: TuiRuntimeState = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored.tun_enabled, Some(true));
        assert_eq!(
            restored.tun_auto_detect_interface_before_enable,
            Some(RouteAutoDetectInterfaceState::FieldMissing)
        );
    }

    #[test]
    fn pending_tun_disable_journal_persists_rollback_state() {
        let state = TuiRuntimeState {
            tun_enabled: Some(false),
            tun_auto_detect_interface_before_enable: Some(
                RouteAutoDetectInterfaceState::RouteMissing,
            ),
            ..TuiRuntimeState::default()
        };
        let json = serde_json::to_string(&state).expect("serializes");
        let restored: TuiRuntimeState = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored.tun_enabled, Some(false));
        assert_eq!(
            restored.tun_auto_detect_interface_before_enable,
            Some(RouteAutoDetectInterfaceState::RouteMissing)
        );
    }

    #[test]
    fn unknown_global_tui_state_fields_are_rejected() {
        let error = serde_json::from_str::<TuiRuntimeState>(
            r#"{
              "legacy_global": null,
              "private_access_profiles": []
            }"#,
        )
        .expect_err("unknown global TUI state settings must be rejected");

        assert!(error.to_string().contains("legacy_global"));
    }

    #[test]
    fn unknown_private_access_service_fields_are_rejected() {
        let error = serde_json::from_str::<TuiRuntimeState>(
            r#"{
              "private_access_profiles": [{
                "id": "hillstone",
                "legacy_profile": null
              }]
            }"#,
        )
        .expect_err("unknown private access profile settings must be rejected");

        assert!(error.to_string().contains("legacy_profile"));
    }
}
