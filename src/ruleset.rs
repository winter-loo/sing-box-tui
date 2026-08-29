use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::CHINA_IP_ROUTING_RULE_SETS;

/// Downloads the China split-routing binary rule-sets and writes them into `ruleset_dir`.
///
/// The download goes through the configured proxy (the sing-box mixed inbound) because the
/// rule-set source (`raw.githubusercontent.com`) is usually unreachable directly. sing-box reads
/// the resulting local files at startup, so a failed or absent network download can never make
/// the proxy service itself fail to start.
pub(crate) async fn download_china_ip_routing_rulesets(
    proxy_server: Option<&str>,
    ruleset_dir: &Path,
) -> Result<()> {
    let mut builder = reqwest::Client::builder();
    if let Some(proxy_server) = proxy_server
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let proxy_url = if proxy_server.contains("://") {
            proxy_server.to_string()
        } else {
            format!("http://{proxy_server}")
        };
        builder = builder.proxy(reqwest::Proxy::all(&proxy_url)?);
    }
    let client = builder
        .build()
        .context("failed to build rule-set download client")?;
    fs::create_dir_all(ruleset_dir)
        .with_context(|| format!("failed to create {}", ruleset_dir.display()))?;
    for (&(tag, url), path) in CHINA_IP_ROUTING_RULE_SETS
        .iter()
        .zip(china_ip_routing_ruleset_paths(ruleset_dir))
    {
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("failed to download rule-set {tag} from {url}"))?;
        let response = response
            .error_for_status()
            .with_context(|| format!("rule-set {tag} download failed with HTTP error"))?;
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("failed to read rule-set {tag} body"))?;
        fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn china_ip_routing_ruleset_paths(ruleset_dir: &Path) -> Vec<PathBuf> {
    CHINA_IP_ROUTING_RULE_SETS
        .iter()
        .map(|(tag, _)| ruleset_dir.join(format!("{tag}.srs")))
        .collect()
}
