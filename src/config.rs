use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::defaults::{
    AUTO_SELECTOR_TAG_ALIASES, BLOCK_TAG_ALIASES, DEFAULT_AD_BLOCK_SELECTOR_TAG,
    DEFAULT_AUTO_SELECTOR_TAG, DEFAULT_BLOCK_TAG, DEFAULT_BYPASS_RULE_SET_PATH,
    DEFAULT_BYPASS_RULE_SET_TAG, DEFAULT_DELAY_TEST_URL, DEFAULT_DIRECT_TAG, DEFAULT_LOCAL_DNS_TAG,
    DEFAULT_REMOTE_DNS_TAG, DEFAULT_SELECTOR_TAG, DIRECT_TAG_ALIASES, SELECTOR_TAG_ALIASES,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DefaultConfigOptions {
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
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
        let mut config: Value = serde_json::from_str(&text)
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
        let mut config: Value = serde_json::from_str(&text)
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
        let mut config: Value = serde_json::from_str(&text)
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
        inbounds.push(json!({
            "type": "tun",
            "tag": "tun-in",
            "address": [
                "172.19.0.1/30",
                "2001:470:f9da:fdfa::1/64"
            ],
            "mtu": 9000,
            "auto_route": true,
            "strict_route": true,
            "stack": "mixed",
            "endpoint_independent_nat": true,
        }));
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
        dns_rules.extend([
            json!({
                "rule_set": "geosite-cn",
                "server": DEFAULT_LOCAL_DNS_TAG,
            }),
            json!({
                "rule_set": "geosite-geolocation-cn",
                "server": DEFAULT_LOCAL_DNS_TAG,
            }),
            json!({
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
            }),
        ]);
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
        route_rules.extend([
            json!({
                "rule_set": "geoip-cn",
                "outbound": DEFAULT_DIRECT_TAG,
            }),
            json!({
                "rule_set": "geosite-cn",
                "outbound": DEFAULT_DIRECT_TAG,
            }),
            json!({
                "rule_set": "geosite-geolocation-cn",
                "outbound": DEFAULT_DIRECT_TAG,
            }),
            json!({
                "rule_set": "AdGuardSDNSFilter",
                "outbound": DEFAULT_AD_BLOCK_SELECTOR_TAG,
            }),
        ]);
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
        route_rule_sets.extend([
            json!({
                "type": "remote",
                "tag": "geoip-cn",
                "format": "binary",
                "url": "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geoip/cn.srs",
                "download_detour": DEFAULT_DIRECT_TAG,
                "update_interval": "30d",
            }),
            json!({
                "type": "remote",
                "tag": "geosite-cn",
                "format": "binary",
                "url": "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/cn.srs",
                "download_detour": DEFAULT_DIRECT_TAG,
                "update_interval": "30d",
            }),
            json!({
                "type": "remote",
                "tag": "geosite-geolocation-cn",
                "format": "binary",
                "url": "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/geolocation-cn.srs",
                "download_detour": DEFAULT_DIRECT_TAG,
                "update_interval": "30d",
            }),
            json!({
                "type": "remote",
                "tag": "geosite-geolocation-!cn",
                "format": "binary",
                "url": "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/geolocation-!cn.srs",
                "download_detour": DEFAULT_DIRECT_TAG,
                "update_interval": "30d",
            }),
            json!({
                "type": "remote",
                "tag": "AdGuardSDNSFilter",
                "format": "binary",
                "url": "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/category-ads-all.srs",
                "download_detour": DEFAULT_DIRECT_TAG,
                "update_interval": "30d",
            }),
        ]);
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
            "external_controller": "127.0.0.1:9992",
            "secret": "",
        })
    });
    let clash_api = clash_api_value
        .as_object_mut()
        .context("existing config experimental.clash_api must be an object")?;
    clash_api
        .entry("external_controller")
        .or_insert_with(|| Value::String("127.0.0.1:9992".to_string()));
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

fn rule_references_rule_set(rule: &Value, tag: &str) -> bool {
    match rule.get("rule_set") {
        Some(Value::String(value)) => value == tag,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(tag)),
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
    use super::{
        DefaultConfigOptions, ProviderNodeSet, build_default_config,
        build_default_config_with_options, build_full_config_with_provider_node_sets,
        ensure_bypass_rule_set_file_for_config, merge_into_existing_config,
    };
    use crate::defaults::DEFAULT_BYPASS_RULE_SET_PATH;
    use serde_json::{Value, json};
    use std::fs;
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
        assert_eq!(
            tun["address"],
            json!(["172.19.0.1/30", "2001:470:f9da:fdfa::1/64"])
        );
        assert_eq!(tun["mtu"], 9000);
        assert_eq!(tun["auto_route"], true);
        assert_eq!(tun["strict_route"], true);
        assert_eq!(tun["stack"], "mixed");
        assert_eq!(tun["endpoint_independent_nat"], true);
        assert!(tun.get("auto_redirect").is_none());
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
