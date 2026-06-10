use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use reqwest::header::USER_AGENT;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::runtime::Builder as TokioRuntimeBuilder;

use crate::clash::{ClashConfig, convert_clash_proxy, is_metadata_entry};
use crate::config::{
    build_full_config, build_full_config_with_provider_groups,
    ensure_bypass_rule_set_file_for_config,
};

pub(crate) fn run_import(
    source: &PathBuf,
    output: Option<&PathBuf>,
    full_config: bool,
    config_path: &PathBuf,
    replace_nodes: bool,
) -> Result<()> {
    let text = fs::read_to_string(source)
        .with_context(|| format!("failed to read Clash proxy file {}", source.display()))?;
    let config: ClashConfig = serde_yaml::from_str(&text).context("failed to parse Clash YAML")?;

    let converted = config
        .proxies
        .into_iter()
        .filter(|entry| !is_metadata_entry(entry))
        .map(convert_clash_proxy)
        .collect::<Result<Vec<_>>>()?;

    let output_value = if full_config {
        build_full_config(config_path, converted, replace_nodes)?
    } else {
        Value::Array(converted)
    };

    let json_text = serde_json::to_string_pretty(&output_value)
        .context("failed to serialize sing-box import output")?;

    if let Some(output) = output {
        fs::write(output, format!("{json_text}\n"))
            .with_context(|| format!("failed to write {}", output.display()))?;
        if full_config {
            ensure_bypass_rule_set_file_for_config(output)?;
        }
        println!("{}", output.display());
    } else {
        println!("{json_text}");
    }

    Ok(())
}

pub(crate) fn run_subscribe_import(
    subscription_url: String,
    output: Option<&PathBuf>,
    config_path: &PathBuf,
    subscription_output: Option<&PathBuf>,
    replace_nodes: bool,
    provider_name: Option<&str>,
    existing_provider_name: Option<&str>,
) -> Result<()> {
    let parsed_url = Url::parse(&subscription_url).with_context(|| {
        format!(
            "invalid subscription URL: {}",
            redact_url(&subscription_url)
        )
    })?;
    let runtime = TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build Tokio runtime for subscription import")?;
    let use_direct_fetch = subscription_url_requires_direct_fetch(&parsed_url);
    let subscription_json = runtime.block_on(async {
        let mut builder = reqwest::Client::builder();
        if use_direct_fetch {
            builder = builder.no_proxy();
        }
        builder
            .build()
            .context("failed to build subscription HTTP client")?
            .get(parsed_url)
            .header(USER_AGENT, "sing-box")
            .send()
            .await
            .context("failed to fetch sing-box subscription URL")?
            .error_for_status()
            .context("subscription server rejected request")?
            .text()
            .await
            .context("failed to read sing-box subscription response")
    })?;

    if subscription_json.trim().is_empty() {
        bail!("subscription response was empty");
    }

    if let Some(path) = subscription_output {
        fs::write(path, format!("{subscription_json}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    let (config, imported_nodes) = if let Some(provider_name) = provider_name {
        build_full_config_from_singbox_subscription_with_provider_groups(
            config_path,
            &subscription_json,
            replace_nodes,
            provider_name,
            existing_provider_name,
        )?
    } else {
        build_full_config_from_singbox_subscription(config_path, &subscription_json, replace_nodes)?
    };
    let config_text =
        serde_json::to_string_pretty(&config).context("failed to serialize merged config")?;

    if let Some(output) = output {
        fs::write(output, format!("{config_text}\n"))
            .with_context(|| format!("failed to write {}", output.display()))?;
        ensure_bypass_rule_set_file_for_config(output)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!(SubscriptionImportOutput {
                subscription_url: redact_url(&subscription_url),
                imported_nodes,
                merged_config_path: output.display().to_string(),
                subscription_output_path: subscription_output
                    .map(|path| path.display().to_string()),
            }))?
        );
    } else {
        println!("{config_text}");
    }

    Ok(())
}

#[derive(Serialize)]
struct SubscriptionImportOutput {
    subscription_url: String,
    imported_nodes: usize,
    merged_config_path: String,
    subscription_output_path: Option<String>,
}

pub(crate) fn build_full_config_from_singbox_subscription(
    config_path: &PathBuf,
    subscription_json: &str,
    replace_nodes: bool,
) -> Result<(Value, usize)> {
    let imported_nodes = extract_mergeable_outbounds_from_singbox_subscription(subscription_json)?;
    let node_count = imported_nodes.len();
    let config = build_full_config(config_path, imported_nodes, replace_nodes)?;
    Ok((config, node_count))
}

pub(crate) fn build_full_config_from_singbox_subscription_with_provider_groups(
    config_path: &PathBuf,
    subscription_json: &str,
    replace_nodes: bool,
    provider_name: &str,
    existing_provider_name: Option<&str>,
) -> Result<(Value, usize)> {
    let imported_nodes = extract_mergeable_outbounds_from_singbox_subscription(subscription_json)?;
    let node_count = imported_nodes.len();
    let config = build_full_config_with_provider_groups(
        config_path,
        imported_nodes,
        replace_nodes,
        provider_name,
        existing_provider_name,
    )?;
    Ok((config, node_count))
}

pub(crate) fn extract_mergeable_outbounds_from_singbox_subscription(
    subscription_json: &str,
) -> Result<Vec<Value>> {
    let payload: Value =
        serde_json::from_str(subscription_json).context("failed to parse sing-box JSON")?;
    let outbounds = payload
        .get("outbounds")
        .and_then(Value::as_array)
        .context("sing-box JSON is missing an outbounds array")?;

    Ok(outbounds
        .iter()
        .filter(|outbound| is_mergeable_subscription_outbound(outbound))
        .cloned()
        .collect())
}

fn subscription_url_requires_direct_fetch(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    host.contains("airtcp") || host.contains("mailrelay")
}

fn is_mergeable_subscription_outbound(outbound: &Value) -> bool {
    let Some(outbound_type) = outbound.get("type").and_then(Value::as_str) else {
        return false;
    };
    if matches!(
        outbound_type,
        "selector" | "urltest" | "direct" | "block" | "dns"
    ) {
        return false;
    }

    let Some(tag) = outbound.get("tag").and_then(Value::as_str) else {
        return false;
    };
    if is_subscription_metadata_tag(tag) {
        return false;
    }
    if matches!(
        tag,
        "手动选择" | "自动选择" | "广告路由" | "国内直连" | "屏蔽" | "dns-out"
    ) {
        return false;
    }
    !tag.contains("如遇不可用请访问")
}

fn is_subscription_metadata_tag(tag: &str) -> bool {
    tag.starts_with("剩余流量")
        || tag.starts_with("距离下次重置剩余")
        || tag.starts_with("套餐到期")
        || tag.contains("官网")
        || tag.contains("刷新订阅")
        || tag.contains("请更换客户端")
        || tag.contains("直连地址")
        || tag.contains("TG群")
        || tag.contains("邀请好友")
}

fn redact_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return value.to_string();
    };
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            if key.eq_ignore_ascii_case("token") {
                (key.into_owned(), "REDACTED".to_string())
            } else {
                (key.into_owned(), value.into_owned())
            }
        })
        .collect::<Vec<_>>();
    if pairs.is_empty() {
        return url.to_string();
    }
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(&key, &value);
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::build_full_config_from_singbox_subscription;

    #[test]
    fn singbox_subscription_extracts_only_mergeable_nodes() {
        let text = r#"{
          "outbounds": [
            {"type":"selector","tag":"手动选择","outbounds":["node-a"]},
            {"type":"urltest","tag":"自动选择","outbounds":["node-a"]},
            {"type":"shadowsocks","tag":"node-a","server":"example.com","server_port":443,"method":"aes-128-gcm","password":"secret"},
            {"type":"vmess","tag":"如遇不可用请访问3.airtcp.us","server":"notice.example.com","server_port":10086,"uuid":"abc"},
            {"type":"vless","tag":"剩余流量：599.96 GB","server":"notice.example.com","server_port":443,"uuid":"abc"},
            {"type":"vmess","tag":"TG群：https://t.me/example","server":"notice.example.com","server_port":10086,"uuid":"abc"},
            {"type":"direct","tag":"国内直连"}
          ]
        }"#;

        let (config, imported_count) = build_full_config_from_singbox_subscription(
            &"/tmp/non-existent-config.json".into(),
            text,
            false,
        )
        .expect("subscription mergeable extraction succeeds");

        assert_eq!(imported_count, 1);
        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert!(outbounds.iter().any(|value| value["tag"] == "node-a"));
        assert!(
            !outbounds
                .iter()
                .any(|value| value["tag"] == "如遇不可用请访问3.airtcp.us")
        );
        assert!(
            !outbounds
                .iter()
                .any(|value| value["tag"] == "剩余流量：599.96 GB")
        );
        assert!(
            !outbounds
                .iter()
                .any(|value| value["tag"] == "TG群：https://t.me/example")
        );
    }

    #[test]
    fn singbox_subscription_filters_reset_countdown_metadata() {
        let text = r#"{
          "outbounds": [
            {"type":"shadowsocks","tag":"node-a","server":"example.com","server_port":443,"method":"aes-128-gcm","password":"secret"},
            {"type":"vless","tag":"距离下次重置剩余：22 天","server":"notice.example.com","server_port":443,"uuid":"abc"}
          ]
        }"#;

        let (config, imported_count) = build_full_config_from_singbox_subscription(
            &"/tmp/non-existent-config.json".into(),
            text,
            false,
        )
        .expect("subscription mergeable extraction succeeds");

        assert_eq!(imported_count, 1);
        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert!(outbounds.iter().any(|value| value["tag"] == "node-a"));
        assert!(
            !outbounds
                .iter()
                .any(|value| value["tag"] == "距离下次重置剩余：22 天")
        );
    }
}
