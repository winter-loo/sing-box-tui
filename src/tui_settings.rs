use anyhow::{Context, Result, bail};

use crate::controller::VerificationTarget;
use crate::defaults::DEFAULT_VERIFICATION_TARGETS;

use super::App;
use super::presentation::{SETTINGS_FIELDS, SettingsField};

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

pub(super) fn default_verification_targets_setting() -> String {
    DEFAULT_VERIFICATION_TARGETS
        .iter()
        .map(|(name, url)| format!("{name}={url}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn parse_verification_targets(input: &str) -> Result<Vec<VerificationTarget>> {
    input
        .split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_verification_target)
        .collect()
}

fn parse_verification_target(input: &str) -> Result<VerificationTarget> {
    let (name, url) = input
        .split_once('=')
        .with_context(|| format!("verification target must be NAME=URL, got {input}"))?;
    let name = name.trim();
    let url = url.trim();
    if name.is_empty() {
        bail!("verification target name cannot be empty");
    }
    if url.is_empty() {
        bail!("verification target URL cannot be empty");
    }
    Ok(VerificationTarget {
        name: name.to_string(),
        url: url.to_string(),
    })
}
