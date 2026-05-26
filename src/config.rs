use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::defaults::{
    AUTO_SELECTOR_TAG_ALIASES, BLOCK_TAG_ALIASES, DEFAULT_AD_BLOCK_SELECTOR_TAG,
    DEFAULT_AUTO_SELECTOR_TAG, DEFAULT_BLOCK_TAG, DEFAULT_DELAY_TEST_URL, DEFAULT_DIRECT_TAG,
    DEFAULT_LOCAL_DNS_TAG, DEFAULT_REMOTE_DNS_TAG, DEFAULT_SELECTOR_TAG, DIRECT_TAG_ALIASES,
    SELECTOR_TAG_ALIASES,
};

pub(crate) fn build_full_config(
    config_path: &PathBuf,
    imported_nodes: Vec<Value>,
    replace_nodes: bool,
) -> Result<Value> {
    if config_path.exists() {
        let text = fs::read_to_string(config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut config: Value = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        merge_into_existing_config(&mut config, imported_nodes, replace_nodes)?;
        Ok(config)
    } else {
        Ok(build_default_config(imported_nodes))
    }
}

pub(crate) fn build_default_config(imported_nodes: Vec<Value>) -> Value {
    let node_tags = collect_tags(&imported_nodes);
    let select_members = with_leading_member(DEFAULT_AUTO_SELECTOR_TAG, &node_tags);

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
                    "server": "8.8.8.8",
                    "server_port": 853,
                    "detour": DEFAULT_SELECTOR_TAG,
                },
                {
                    "type": "tls",
                    "tag": DEFAULT_LOCAL_DNS_TAG,
                    "server": "223.5.5.5",
                    "server_port": 853,
                }
            ],
            "rules": [
                {
                    "clash_mode": "全局",
                    "server": DEFAULT_REMOTE_DNS_TAG,
                },
                {
                    "clash_mode": "直连",
                    "server": DEFAULT_LOCAL_DNS_TAG,
                },
                {
                    "rule_set": "geosite-cn",
                    "server": DEFAULT_LOCAL_DNS_TAG,
                },
                {
                    "rule_set": "geosite-geolocation-cn",
                    "server": DEFAULT_LOCAL_DNS_TAG,
                },
                {
                    "type": "logical",
                    "mode": "and",
                    "rules": [
                        {
                            "rule_set": "geosite-geolocation-!cn",
                            "invert": true,
                        },
                        {
                            "rule_set": "geoip-cn",
                        }
                    ],
                    "server": DEFAULT_REMOTE_DNS_TAG,
                    "client_subnet": "114.114.114.114/24",
                },
                {
                    "clash_mode": "规则",
                    "server": DEFAULT_REMOTE_DNS_TAG,
                }
            ],
            "strategy": "ipv4_only",
            "independent_cache": false,
        },
        "inbounds": [
            {
                "type": "tun",
                "mtu": 9000,
                "address": [
                    "172.19.0.1/30",
                    "2001:0470:f9da:fdfa::1/64",
                ],
                "auto_route": true,
                "auto_redirect": true,
                "strict_route": true,
                "stack": "mixed",
                "sniff": true,
                "endpoint_independent_nat": true,
                "domain_strategy": "ipv4_only",
            },
            {
                "type": "mixed",
                "listen": "::",
                "listen_port": 5780,
                "sniff": true,
                "domain_strategy": "ipv4_only",
                "set_system_proxy": false,
            }
        ],
        "outbounds": outbounds,
        "route": {
            "default_domain_resolver": {
                "server": DEFAULT_LOCAL_DNS_TAG,
                "strategy": "ipv4_only",
            },
            "rules": [
                {
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
                },
                {
                    "clash_mode": "直连",
                    "outbound": DEFAULT_DIRECT_TAG,
                },
                {
                    "clash_mode": "全局",
                    "outbound": DEFAULT_SELECTOR_TAG,
                },
                {
                    "ip_is_private": true,
                    "outbound": DEFAULT_DIRECT_TAG,
                },
                {
                    "rule_set": "geoip-cn",
                    "outbound": DEFAULT_DIRECT_TAG,
                },
                {
                    "rule_set": "geosite-cn",
                    "outbound": DEFAULT_DIRECT_TAG,
                },
                {
                    "rule_set": "geosite-geolocation-cn",
                    "outbound": DEFAULT_DIRECT_TAG,
                },
                {
                    "rule_set": "AdGuardSDNSFilter",
                    "outbound": DEFAULT_AD_BLOCK_SELECTOR_TAG,
                },
                {
                    "clash_mode": "规则",
                    "outbound": DEFAULT_SELECTOR_TAG,
                }
            ],
            "rule_set": [
                {
                    "type": "remote",
                    "tag": "geoip-cn",
                    "format": "binary",
                    "url": "https://sjcdjf01.airapp.link/theme/rules/geoip-cn.srs",
                    "download_detour": DEFAULT_DIRECT_TAG,
                    "update_interval": "30d",
                },
                {
                    "type": "remote",
                    "tag": "geosite-cn",
                    "format": "binary",
                    "url": "https://sjcdjf01.airapp.link/theme/rules/geosite-cn.srs",
                    "download_detour": DEFAULT_DIRECT_TAG,
                    "update_interval": "30d",
                },
                {
                    "type": "remote",
                    "tag": "geosite-geolocation-cn",
                    "format": "binary",
                    "url": "https://sjcdjf01.airapp.link/theme/rules/geosite-geolocation-cn.srs",
                    "download_detour": DEFAULT_DIRECT_TAG,
                    "update_interval": "30d",
                },
                {
                    "type": "remote",
                    "tag": "geosite-geolocation-!cn",
                    "format": "binary",
                    "url": "https://sjcdjf01.airapp.link/theme/rules/geosite-geolocation-!cn.srs",
                    "download_detour": DEFAULT_DIRECT_TAG,
                    "update_interval": "30d",
                },
                {
                    "type": "remote",
                    "tag": "AdGuardSDNSFilter",
                    "format": "binary",
                    "url": "https://sjcdjf01.airapp.link/theme/rules/AdGuardSDNSFilter.srs",
                    "download_detour": DEFAULT_DIRECT_TAG,
                    "update_interval": "30d",
                }
            ],
            "auto_detect_interface": true,
        },
        "experimental": {
            "cache_file": {
                "enabled": true,
                "store_rdrc": true,
            },
            "clash_api": {
                "external_controller": "0.0.0.0:9090",
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
    let select_members = with_leading_member(&auto_tag, &node_tags);

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
            "external_controller": "127.0.0.1:9090",
            "secret": "",
        })
    });
    let clash_api = clash_api_value
        .as_object_mut()
        .context("existing config experimental.clash_api must be an object")?;
    clash_api
        .entry("external_controller")
        .or_insert_with(|| Value::String("127.0.0.1:9090".to_string()));
    clash_api
        .entry("secret")
        .or_insert_with(|| Value::String(String::new()));

    Ok(())
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

fn with_leading_member(first: &str, tags: &[String]) -> Vec<String> {
    let mut members = Vec::with_capacity(tags.len() + 1);
    members.push(first.to_string());
    members.extend(tags.iter().cloned());
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
        if let Some(member) = value.as_str() {
            if merged.insert(member.to_string()) {
                values.push(Value::String(member.to_string()));
            }
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
    use super::{build_default_config, merge_into_existing_config};
    use serde_json::{Value, json};

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
        assert!(outbounds.iter().any(|value| value["tag"] == "手动选择"));
        assert!(outbounds.iter().any(|value| value["tag"] == "自动选择"));
        assert!(outbounds.iter().any(|value| value["tag"] == "广告路由"));
        assert!(outbounds.iter().any(|value| value["tag"] == "node-a"));
        assert_eq!(config["dns"]["servers"][0]["type"], "tls");
        assert_eq!(
            config["route"]["default_domain_resolver"]["server"],
            "local"
        );
        assert_eq!(config["route"]["rules"][0]["action"], "hijack-dns");
        assert!(config["route"].get("final").is_none());
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
        assert!(members.contains(&Value::String("node-a".to_string())));
        assert_eq!(config["route"]["final"], "existing-node");
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
        assert!(members.contains(&Value::String("new-node".to_string())));
    }
}
