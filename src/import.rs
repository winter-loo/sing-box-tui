use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::clash::{ClashConfig, convert_clash_proxy, is_metadata_entry};
use crate::config::build_full_config;

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
        println!("{}", output.display());
    } else {
        println!("{json_text}");
    }

    Ok(())
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

fn extract_mergeable_outbounds_from_singbox_subscription(
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
    if matches!(
        tag,
        "手动选择" | "自动选择" | "广告路由" | "国内直连" | "屏蔽" | "dns-out"
    ) {
        return false;
    }
    !tag.contains("如遇不可用请访问")
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
    }
}
