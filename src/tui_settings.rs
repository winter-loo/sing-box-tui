use std::net::SocketAddrV4;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::event::KeyCode;

use crate::config::{china_ip_routing_ruleset_dir, set_china_ip_routing};
use crate::private_access_session::{
    PrivateAccessMode, PrivateAccessProfileRuntime, load_manifest_for_profile,
    parse_private_access_mode,
};
use crate::ruleset::download_china_ip_routing_rulesets;

use super::App;
use super::presentation::{
    SETTINGS_FIELDS, SettingsEditState, SettingsField, settings_field_label, truncate_for_width,
};
use super::verification::parse_verification_targets;

#[cfg(test)]
#[path = "tui_settings_tests.rs"]
mod behavior_tests;

fn private_access_profile_settings_locked(profile: &PrivateAccessProfileRuntime) -> bool {
    profile.settings_locked()
}

pub(super) fn visible_settings_fields(app: &App) -> Vec<SettingsField> {
    SETTINGS_FIELDS
        .iter()
        .copied()
        .filter(|field| {
            !is_private_access_settings_field(*field) || app.private_access.is_configured()
        })
        .filter(|field| {
            *field != SettingsField::PrivateAccessUseInternetProxy
                || app
                    .private_access
                    .focused_opt()
                    .is_some_and(|profile| profile.manifest.id == "sonicwall")
        })
        .collect()
}

pub(super) fn is_private_access_settings_field(field: SettingsField) -> bool {
    matches!(
        field,
        SettingsField::PrivateAccessProfile
            | SettingsField::PrivateAccessManifestPath
            | SettingsField::PrivateAccessMode
            | SettingsField::PrivateAccessServer
            | SettingsField::PrivateAccessPort
            | SettingsField::PrivateAccessUsername
            | SettingsField::PrivateAccessPassword
            | SettingsField::PrivateAccessPasswordEnv
            | SettingsField::PrivateAccessBridgeListen
            | SettingsField::PrivateAccessUseInternetProxy
            | SettingsField::PrivateAccessTlsVerify
    )
}

pub(super) fn settings_field_value(app: &App, field: SettingsField) -> String {
    match field {
        SettingsField::BenchmarkUrl => app.benchmark_url.clone(),
        SettingsField::BenchmarkTimeoutMs => app.benchmark_timeout_ms.to_string(),
        SettingsField::RequestTimeoutSec => app.benchmark_request_timeout.to_string(),
        SettingsField::MaxConcurrency => app.benchmark_max_concurrency.to_string(),
        SettingsField::VerifyTargets => app.verify_targets.clone(),
        SettingsField::AutoPickThresholdMs => app.auto_select_threshold_ms.to_string(),
        SettingsField::AutoPickIntervalSec => app.auto_select_interval.as_secs().to_string(),
        SettingsField::SystemProxyServer => app.system_proxy.server().to_string(),
        SettingsField::ChinaIpRouting => app.china_ip_routing_enabled.to_string(),
        SettingsField::PrivateAccessProfile => app
            .private_access
            .focused_opt()
            .map(|profile| profile.id.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessManifestPath => app
            .private_access
            .focused_opt()
            .map(|profile| profile.manifest_path.clone().unwrap_or_default())
            .unwrap_or_default(),
        SettingsField::PrivateAccessMode => app
            .private_access
            .focused_opt()
            .map(|profile| profile.mode.as_str().to_string())
            .unwrap_or_default(),
        SettingsField::PrivateAccessServer => app
            .private_access
            .focused_opt()
            .map(|profile| profile.server.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessPort => app
            .private_access
            .focused_opt()
            .map(|profile| profile.port.to_string())
            .unwrap_or_default(),
        SettingsField::PrivateAccessUsername => app
            .private_access
            .focused_opt()
            .map(|profile| profile.username.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessPassword => app
            .private_access
            .focused_opt()
            .map(|profile| profile.password.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessPasswordEnv => app
            .private_access
            .focused_opt()
            .map(|profile| profile.password_env.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessBridgeListen => app
            .private_access
            .focused_opt()
            .map(|profile| profile.bridge_listen.clone())
            .unwrap_or_default(),
        SettingsField::PrivateAccessUseInternetProxy => app
            .private_access
            .focused_opt()
            .map(|profile| profile.use_internet_proxy.to_string())
            .unwrap_or_default(),
        SettingsField::PrivateAccessTlsVerify => app
            .private_access
            .focused_opt()
            .map(|profile| profile.tls_verify.to_string())
            .unwrap_or_default(),
    }
}

pub(super) fn settings_field_display_value(app: &App, field: SettingsField) -> String {
    settings_field_value(app, field)
}

pub(super) fn parse_positive<T>(value: &str) -> Result<T>
where
    T: std::str::FromStr + PartialOrd + From<u8>,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let parsed = value.parse::<T>().context("value must be a number")?;
    if parsed <= T::from(0) {
        bail!("value must be greater than 0");
    }
    Ok(parsed)
}

pub(super) fn parse_bool_setting(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("value must be true or false"),
    }
}

pub(super) fn normalize_http_connect_proxy(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let value = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("HTTP://"))
        .unwrap_or(value)
        .trim_end_matches('/');
    (!value.is_empty()).then(|| value.to_string())
}

pub(super) type SonicwallHttpConnectSettings = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub(super) fn sonicwall_http_connect_settings(
    use_internet_proxy: bool,
    system_proxy_server: &str,
    outbound_context: Option<String>,
    controller: &str,
    selector: Option<String>,
) -> SonicwallHttpConnectSettings {
    if !use_internet_proxy {
        return (None, None, None, None);
    }
    (
        normalize_http_connect_proxy(system_proxy_server),
        outbound_context,
        Some(controller.to_string()),
        selector,
    )
}

pub(super) fn normalize_optional_setting(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl App {
    pub(super) fn open_settings_panel(&mut self) {
        self.show_settings = true;
        self.settings_edit = None;
        let field_count = visible_settings_fields(self).len();
        self.settings_index = self.settings_index.min(field_count.saturating_sub(1));
        self.flash = None;
        self.set_status_only("Showing settings");
    }

    pub(super) fn handle_settings_key(&mut self, code: KeyCode) -> Result<bool> {
        if self.settings_edit.is_some() {
            return self.handle_settings_edit_key(code);
        }
        let fields = visible_settings_fields(self);
        self.settings_index = self.settings_index.min(fields.len().saturating_sub(1));
        match code {
            KeyCode::Esc | KeyCode::Char('o') => {
                self.show_settings = false;
                self.settings_error = None;
                self.set_status_only("Settings closed");
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_error = None;
                self.settings_index = (self.settings_index + 1).min(fields.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_error = None;
                self.settings_index = self.settings_index.saturating_sub(1);
            }
            KeyCode::Enter => {
                let field = fields[self.settings_index];
                self.settings_error = None;
                self.settings_edit = Some(SettingsEditState {
                    field,
                    input: settings_field_value(self, field),
                    error: None,
                });
            }
            KeyCode::Char('q') => return Ok(false),
            _ => {}
        }
        Ok(true)
    }

    fn handle_settings_edit_key(&mut self, code: KeyCode) -> Result<bool> {
        let Some(edit) = self.settings_edit.as_mut() else {
            return Ok(true);
        };
        match code {
            KeyCode::Esc => {
                self.settings_edit = None;
                self.settings_error = None;
            }
            KeyCode::Enter => {
                let edit = self.settings_edit.take().expect("settings edit exists");
                if let Err(error) = self.apply_settings_value(edit.field, edit.input.clone()) {
                    let message = error.to_string();
                    self.settings_edit = Some(SettingsEditState {
                        error: Some(message),
                        ..edit
                    });
                }
            }
            KeyCode::Backspace => {
                edit.input.pop();
                edit.error = None;
            }
            KeyCode::Char(ch) => {
                edit.input.push(ch);
                edit.error = None;
            }
            _ => {}
        }
        Ok(true)
    }

    pub(super) fn apply_settings_value(
        &mut self,
        field: SettingsField,
        input: String,
    ) -> Result<()> {
        if is_private_access_settings_field(field) && !self.private_access.is_configured() {
            bail!("Private Access is not configured");
        }
        let value = input.trim();
        match field {
            SettingsField::BenchmarkUrl => {
                if value.is_empty() {
                    bail!("latency URL cannot be empty");
                }
                self.benchmark_url = value.to_string();
            }
            SettingsField::BenchmarkTimeoutMs => self.benchmark_timeout_ms = parse_positive(value)?,
            SettingsField::RequestTimeoutSec => {
                self.benchmark_request_timeout = value
                    .parse::<f64>()
                    .context("request timeout must be a number")?;
                if self.benchmark_request_timeout <= 0.0 {
                    bail!("request timeout must be greater than 0");
                }
            }
            SettingsField::MaxConcurrency => {
                self.benchmark_max_concurrency = parse_positive(value)?
            }
            SettingsField::VerifyTargets => {
                if !value.is_empty() {
                    parse_verification_targets(value)?;
                }
                self.verify_targets = value.to_string();
            }
            SettingsField::AutoPickThresholdMs => {
                self.auto_select_threshold_ms = parse_positive(value)?
            }
            SettingsField::AutoPickIntervalSec => {
                let seconds: u64 = parse_positive(value)?;
                self.auto_select_interval = Duration::from_secs(seconds);
            }
            SettingsField::SystemProxyServer => {
                if value.is_empty() {
                    bail!("system proxy server cannot be empty");
                }
                self.system_proxy.override_server(value.to_string());
            }
            SettingsField::ChinaIpRouting => {
                let enable = parse_bool_setting(value)?;
                if enable {
                    let ruleset_dir = china_ip_routing_ruleset_dir(&self.system_proxy_config_path);
                    let proxy_server = self.system_proxy.server().to_string();
                    self.client
                        .runtime
                        .block_on(download_china_ip_routing_rulesets(
                            Some(&proxy_server),
                            &ruleset_dir,
                        ))?;
                }
                let changed = set_china_ip_routing(&self.system_proxy_config_path, enable)?;
                self.china_ip_routing_enabled = enable;
                self.china_ip_routing_explicit = true;
                self.save_runtime_state()?;
                if changed {
                    let receipt = self.restart_managed_sing_box()?;
                    let label = if enable { "enabled" } else { "disabled" };
                    match receipt.observe_controller(&self.client) {
                        Ok(()) => self.set_status_with_flash(format!(
                            "China IP routing {label}; sing-box restarted"
                        )),
                        Err(error) => self.set_status_with_flash(format!(
                            "China IP routing {label}; controller not ready: {}",
                            truncate_for_width(&format!("{error:#}"), 60)
                        )),
                    }
                } else {
                    self.set_status_with_flash(format!(
                        "China IP routing already {}",
                        if enable { "enabled" } else { "disabled" }
                    ));
                }
                return Ok(());
            }
            SettingsField::PrivateAccessProfile => {
                self.private_access.set_focus_by_id(value)?;
            }
            SettingsField::PrivateAccessManifestPath => {
                if private_access_profile_settings_locked(self.private_access.focused()) {
                    bail!("disconnect Private Access before changing service manifest");
                }
                let profile_id = self.private_access.focused().id.clone();
                let manifest_path = normalize_optional_setting(Some(value.to_string()));
                let manifest = load_manifest_for_profile(&profile_id, manifest_path.as_deref())?;
                let focused = self.private_access.focused_mut();
                focused.manifest_path = manifest_path;
                focused.manifest = manifest;
            }
            SettingsField::PrivateAccessMode => {
                if private_access_profile_settings_locked(self.private_access.focused()) {
                    bail!("disconnect Private Access before changing data plane mode");
                }
                if self.private_access.focused().manifest.id == "sonicwall" {
                    if parse_private_access_mode(value)? != PrivateAccessMode::Tun {
                        bail!("SonicWall private access supports TUN mode only");
                    }
                    self.private_access.focused_mut().mode = PrivateAccessMode::Tun;
                } else {
                    self.private_access.focused_mut().mode = parse_private_access_mode(value)?;
                }
            }
            SettingsField::PrivateAccessServer => {
                self.private_access.focused_mut().server = value.to_string();
            }
            SettingsField::PrivateAccessPort => {
                self.private_access.focused_mut().port = parse_positive(value)?
            }
            SettingsField::PrivateAccessUsername => {
                self.private_access.focused_mut().username = value.to_string();
            }
            SettingsField::PrivateAccessPassword => {
                self.private_access.focused_mut().password = value.to_string();
            }
            SettingsField::PrivateAccessPasswordEnv => {
                self.private_access.focused_mut().password_env = value.to_string();
            }
            SettingsField::PrivateAccessBridgeListen => {
                value
                    .parse::<SocketAddrV4>()
                    .context("bridge listen must be an IPv4:PORT address")?;
                self.private_access.focused_mut().bridge_listen = value.to_string();
            }
            SettingsField::PrivateAccessUseInternetProxy => {
                if private_access_profile_settings_locked(self.private_access.focused()) {
                    bail!("disconnect Private Access before changing SonicWall transport");
                }
                if self.private_access.focused().manifest.id != "sonicwall" {
                    bail!("Internet proxy transport is only configurable for SonicWall");
                }
                self.private_access.focused_mut().use_internet_proxy = parse_bool_setting(value)?;
            }
            SettingsField::PrivateAccessTlsVerify => {
                self.private_access.focused_mut().tls_verify = parse_bool_setting(value)?;
            }
        }
        self.save_runtime_state()?;
        self.ensure_auto_pick_background_worker_after_state_change()?;
        self.set_status_only(format!("Saved {}", settings_field_label(field)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_http_connect_proxy, parse_bool_setting, parse_positive,
        sonicwall_http_connect_settings,
    };

    #[test]
    fn sonicwall_http_connect_proxy_uses_tui_mixed_inbound() {
        assert_eq!(
            normalize_http_connect_proxy("127.0.0.1:6780").as_deref(),
            Some("127.0.0.1:6780")
        );
        assert_eq!(
            normalize_http_connect_proxy("http://127.0.0.1:6780/").as_deref(),
            Some("127.0.0.1:6780")
        );
        assert_eq!(normalize_http_connect_proxy("  "), None);
    }

    #[test]
    fn sonicwall_transport_setting_is_exclusive() {
        let direct = sonicwall_http_connect_settings(
            false,
            "127.0.0.1:6780",
            Some("manual -> node-a".to_string()),
            "http://127.0.0.1:9992",
            Some("manual".to_string()),
        );
        assert_eq!(direct, (None, None, None, None));

        let proxied = sonicwall_http_connect_settings(
            true,
            "127.0.0.1:6780",
            Some("manual -> node-a".to_string()),
            "http://127.0.0.1:9992",
            Some("manual".to_string()),
        );
        assert_eq!(
            proxied,
            (
                Some("127.0.0.1:6780".to_string()),
                Some("manual -> node-a".to_string()),
                Some("http://127.0.0.1:9992".to_string()),
                Some("manual".to_string()),
            )
        );
    }

    #[test]
    fn setting_parsers_validate_their_own_input_contracts() {
        assert_eq!(parse_positive::<u64>("12").unwrap(), 12);
        assert!(parse_positive::<u64>("0").is_err());
        assert_eq!(parse_bool_setting("yes").unwrap(), true);
        assert_eq!(parse_bool_setting("off").unwrap(), false);
        assert!(parse_bool_setting("maybe").is_err());
    }
}
