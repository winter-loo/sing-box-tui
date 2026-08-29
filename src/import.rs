use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::Url;
use reqwest::header::USER_AGENT;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::runtime::Builder as TokioRuntimeBuilder;

use crate::atomic_file::write_atomic;
use crate::clash::{ClashConfig, convert_clash_proxy, is_metadata_entry};
use crate::config::{
    DefaultConfigOptions, build_full_config_with_options,
    build_full_config_with_provider_groups_and_options, ensure_bypass_rule_set_file_for_config,
    resolved_bypass_rule_set_path_for_config,
};
use crate::config_mutation::{
    commit_active_node_config, lock_config_mutation_for, paths_refer_to_same_target,
};
use crate::node_quality_path::default_benchmark_db_path_for_config;

pub(crate) fn run_import(
    source: &PathBuf,
    output: Option<&PathBuf>,
    full_config: bool,
    config_path: &PathBuf,
    replace_nodes: bool,
    include_geosite_rules: bool,
    include_tun_mode: bool,
) -> Result<()> {
    preflight_import_paths(source, output, full_config, config_path)?;
    let text = fs::read_to_string(source)
        .with_context(|| format!("failed to read Clash proxy file {}", source.display()))?;
    let config: ClashConfig = serde_yaml::from_str(&text).context("failed to parse Clash YAML")?;

    let converted = config
        .proxies
        .into_iter()
        .filter(|entry| !is_metadata_entry(entry))
        .map(convert_clash_proxy)
        .collect::<Result<Vec<_>>>()?;

    if full_config && let Some(output) = output {
        write_imported_nodes_full_config(
            config_path,
            output,
            converted,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
        )?;
        println!("{}", output.display());
        return Ok(());
    }

    let _config_guard = if full_config {
        Some(lock_config_mutation_for(config_path)?)
    } else {
        None
    };
    let output_value = if full_config {
        build_full_config_with_options(
            config_path,
            converted,
            replace_nodes,
            DefaultConfigOptions {
                include_geosite_rules,
                include_tun_mode,
            },
        )?
    } else {
        Value::Array(converted)
    };

    let json_text = serde_json::to_string_pretty(&output_value)
        .context("failed to serialize sing-box import output")?;

    if let Some(output) = output {
        if full_config {
            ensure_bypass_rule_set_file_for_config(output)?;
        }
        write_atomic(output, format!("{json_text}\n").as_bytes())
            .with_context(|| format!("failed to write {}", output.display()))?;
        println!("{}", output.display());
    } else {
        println!("{json_text}");
    }

    Ok(())
}

fn preflight_import_paths(
    source: &Path,
    output: Option<&PathBuf>,
    full_config: bool,
    config_path: &Path,
) -> Result<()> {
    let Some(output) = output else {
        return Ok(());
    };
    let active_output = paths_refer_to_same_target(config_path, output)?;
    if !full_config && active_output {
        bail!("raw import output must not overwrite the active sing-box config");
    }
    let database_path = default_benchmark_db_path_for_config(config_path)?;
    let bypass_path = if full_config {
        Some(resolved_bypass_rule_set_path_for_config(output)?)
    } else {
        None
    };
    let mut auxiliary = vec![("Clash import source", source)];
    if !active_output {
        auxiliary.push(("import output", output.as_path()));
    }
    if let Some(path) = bypass_path.as_deref() {
        auxiliary.push(("bypass rule-set", path));
    }
    crate::node_quality_path::ensure_active_config_paths_are_distinct(
        config_path,
        &database_path,
        &auxiliary,
    )
}

pub(crate) fn commit_imported_nodes_to_active_config(
    config_path: &PathBuf,
    active_output: &Path,
    database_path: &Path,
    imported_nodes: Vec<Value>,
    replace_nodes: bool,
    include_geosite_rules: bool,
    include_tun_mode: bool,
) -> Result<()> {
    if !paths_refer_to_same_target(config_path, active_output)? {
        bail!("active config commit destination does not match --config target");
    }
    commit_active_node_config(
        active_output,
        database_path,
        || {
            build_full_config_with_options(
                config_path,
                imported_nodes,
                replace_nodes,
                DefaultConfigOptions {
                    include_geosite_rules,
                    include_tun_mode,
                },
            )
        },
        || ensure_bypass_rule_set_file_for_config(active_output).map(|_| ()),
        || Ok(None),
    )?;
    Ok(())
}

pub(crate) fn write_imported_nodes_full_config(
    config_path: &PathBuf,
    output: &Path,
    imported_nodes: Vec<Value>,
    replace_nodes: bool,
    include_geosite_rules: bool,
    include_tun_mode: bool,
) -> Result<()> {
    let database_path = default_benchmark_db_path_for_config(config_path)?;
    let bypass_path = resolved_bypass_rule_set_path_for_config(output)?;
    if paths_refer_to_same_target(config_path, output)? {
        crate::node_quality_path::ensure_active_config_paths_are_distinct(
            config_path,
            &database_path,
            &[("bypass rule-set", bypass_path.as_path())],
        )?;
        return commit_imported_nodes_to_active_config(
            config_path,
            output,
            &database_path,
            imported_nodes,
            replace_nodes,
            include_geosite_rules,
            include_tun_mode,
        );
    }

    crate::node_quality_path::ensure_active_config_paths_are_distinct(
        config_path,
        &database_path,
        &[
            ("merged import output", output),
            ("bypass rule-set", bypass_path.as_path()),
        ],
    )?;

    let _config_guard = lock_config_mutation_for(config_path)?;
    let config = build_full_config_with_options(
        config_path,
        imported_nodes,
        replace_nodes,
        DefaultConfigOptions {
            include_geosite_rules,
            include_tun_mode,
        },
    )?;
    let contents = serde_json::to_string_pretty(&config)
        .context("failed to serialize sing-box import output")?;
    ensure_bypass_rule_set_file_for_config(output)?;
    write_atomic(output, format!("{contents}\n").as_bytes())
        .with_context(|| format!("failed to write {}", output.display()))
}

pub(crate) struct SubscribeImportOptions {
    pub(crate) subscription_url: String,
    pub(crate) output: Option<PathBuf>,
    pub(crate) config_path: PathBuf,
    pub(crate) subscription_output: Option<PathBuf>,
    pub(crate) replace_nodes: bool,
    pub(crate) include_geosite_rules: bool,
    pub(crate) include_tun_mode: bool,
    pub(crate) provider_name: Option<String>,
    pub(crate) existing_provider_name: Option<String>,
}

pub(crate) fn run_subscribe_import(options: SubscribeImportOptions) -> Result<()> {
    let SubscribeImportOptions {
        subscription_url,
        output,
        config_path,
        subscription_output,
        replace_nodes,
        include_geosite_rules,
        include_tun_mode,
        provider_name,
        existing_provider_name,
    } = options;
    let output = output.as_ref();
    let subscription_output = subscription_output.as_ref();
    let active_output = output
        .map(|path| paths_refer_to_same_target(&config_path, path))
        .transpose()?
        .unwrap_or(false);
    let bypass_path = output
        .map(|path| resolved_bypass_rule_set_path_for_config(path))
        .transpose()?;
    let has_file_output = output.is_some() || subscription_output.is_some();
    let resolved_database_path = has_file_output
        .then(|| default_benchmark_db_path_for_config(&config_path))
        .transpose()?;
    let database_path = if active_output {
        let mut auxiliary = subscription_output
            .map(|path| vec![("subscription output", path.as_path())])
            .unwrap_or_default();
        if let Some(path) = bypass_path.as_deref() {
            auxiliary.push(("bypass rule-set", path));
        }
        crate::node_quality_path::ensure_active_config_paths_are_distinct(
            &config_path,
            resolved_database_path
                .as_deref()
                .context("active subscription import requires node-quality path binding")?,
            &auxiliary,
        )?;
        resolved_database_path
    } else if let Some(resolved_database_path) = resolved_database_path {
        let mut paths = Vec::new();
        if let Some(path) = output {
            paths.push(("merged subscription output", path.as_path()));
        }
        if let Some(path) = subscription_output {
            paths.push(("subscription output", path.as_path()));
        }
        if let Some(path) = bypass_path.as_deref() {
            paths.push(("bypass rule-set", path));
        }
        crate::node_quality_path::ensure_active_config_paths_are_distinct(
            &config_path,
            &resolved_database_path,
            &paths,
        )?;
        None
    } else {
        None
    };
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
        write_atomic(path, format!("{subscription_json}\n").as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    if let Some(output) = output {
        let imported_nodes = if active_output {
            commit_subscription_payload_to_active_config(
                &config_path,
                output,
                database_path
                    .as_deref()
                    .context("active subscription import requires node-quality storage")?,
                SubscriptionConfigRequest {
                    subscription_json: &subscription_json,
                    replace_nodes,
                    config_options: DefaultConfigOptions {
                        include_geosite_rules,
                        include_tun_mode,
                    },
                    provider_name: provider_name.as_deref(),
                    existing_provider_name: existing_provider_name.as_deref(),
                },
            )?
        } else {
            let _config_guard = lock_config_mutation_for(&config_path)?;
            let (config, imported_nodes) = build_subscribe_config(
                &config_path,
                SubscriptionConfigRequest {
                    subscription_json: &subscription_json,
                    replace_nodes,
                    config_options: DefaultConfigOptions {
                        include_geosite_rules,
                        include_tun_mode,
                    },
                    provider_name: provider_name.as_deref(),
                    existing_provider_name: existing_provider_name.as_deref(),
                },
            )?;
            let config_text = serde_json::to_string_pretty(&config)
                .context("failed to serialize merged config")?;
            ensure_bypass_rule_set_file_for_config(output)?;
            write_atomic(output, format!("{config_text}\n").as_bytes())
                .with_context(|| format!("failed to write {}", output.display()))?;
            imported_nodes
        };
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
        let _config_guard = lock_config_mutation_for(&config_path)?;
        let (config, _) = build_subscribe_config(
            &config_path,
            SubscriptionConfigRequest {
                subscription_json: &subscription_json,
                replace_nodes,
                config_options: DefaultConfigOptions {
                    include_geosite_rules,
                    include_tun_mode,
                },
                provider_name: provider_name.as_deref(),
                existing_provider_name: existing_provider_name.as_deref(),
            },
        )?;
        let config_text =
            serde_json::to_string_pretty(&config).context("failed to serialize merged config")?;
        println!("{config_text}");
    }

    Ok(())
}

pub(crate) struct SubscriptionConfigRequest<'a> {
    pub(crate) subscription_json: &'a str,
    pub(crate) replace_nodes: bool,
    pub(crate) config_options: DefaultConfigOptions,
    pub(crate) provider_name: Option<&'a str>,
    pub(crate) existing_provider_name: Option<&'a str>,
}

impl<'a> SubscriptionConfigRequest<'a> {
    pub(crate) fn without_provider(
        subscription_json: &'a str,
        replace_nodes: bool,
        config_options: DefaultConfigOptions,
    ) -> Self {
        Self {
            subscription_json,
            replace_nodes,
            config_options,
            provider_name: None,
            existing_provider_name: None,
        }
    }
}

pub(crate) fn commit_subscription_payload_to_active_config(
    config_path: &PathBuf,
    active_output: &Path,
    database_path: &Path,
    request: SubscriptionConfigRequest<'_>,
) -> Result<usize> {
    if !paths_refer_to_same_target(config_path, active_output)? {
        bail!("active config commit destination does not match --config target");
    }
    let imported_nodes = Cell::new(0);
    commit_active_node_config(
        active_output,
        database_path,
        || {
            let (config, count) = build_subscribe_config(config_path, request)?;
            imported_nodes.set(count);
            Ok(config)
        },
        || ensure_bypass_rule_set_file_for_config(active_output).map(|_| ()),
        || Ok(None),
    )?;
    Ok(imported_nodes.get())
}

fn build_subscribe_config(
    config_path: &PathBuf,
    request: SubscriptionConfigRequest<'_>,
) -> Result<(Value, usize)> {
    if let Some(provider_name) = request.provider_name {
        build_full_config_from_singbox_subscription_with_provider_groups(
            config_path,
            request.subscription_json,
            request.replace_nodes,
            provider_name,
            request.existing_provider_name,
            request.config_options,
        )
    } else {
        build_full_config_from_singbox_subscription_with_options(
            config_path,
            request.subscription_json,
            request.replace_nodes,
            request.config_options,
        )
    }
}

#[derive(Serialize)]
struct SubscriptionImportOutput {
    subscription_url: String,
    imported_nodes: usize,
    merged_config_path: String,
    subscription_output_path: Option<String>,
}

#[cfg(test)]
pub(crate) fn build_full_config_from_singbox_subscription(
    config_path: &PathBuf,
    subscription_json: &str,
    replace_nodes: bool,
) -> Result<(Value, usize)> {
    build_full_config_from_singbox_subscription_with_options(
        config_path,
        subscription_json,
        replace_nodes,
        DefaultConfigOptions::default(),
    )
}

pub(crate) fn build_full_config_from_singbox_subscription_with_options(
    config_path: &PathBuf,
    subscription_json: &str,
    replace_nodes: bool,
    default_config_options: DefaultConfigOptions,
) -> Result<(Value, usize)> {
    let imported_nodes = extract_mergeable_outbounds_from_singbox_subscription(subscription_json)?;
    let node_count = imported_nodes.len();
    let config = build_full_config_with_options(
        config_path,
        imported_nodes,
        replace_nodes,
        default_config_options,
    )?;
    Ok((config, node_count))
}

pub(crate) fn build_full_config_from_singbox_subscription_with_provider_groups(
    config_path: &PathBuf,
    subscription_json: &str,
    replace_nodes: bool,
    provider_name: &str,
    existing_provider_name: Option<&str>,
    default_config_options: DefaultConfigOptions,
) -> Result<(Value, usize)> {
    let imported_nodes = extract_mergeable_outbounds_from_singbox_subscription(subscription_json)?;
    let node_count = imported_nodes.len();
    let config = build_full_config_with_provider_groups_and_options(
        config_path,
        imported_nodes,
        replace_nodes,
        provider_name,
        existing_provider_name,
        default_config_options,
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
    use super::{
        SubscribeImportOptions, build_full_config_from_singbox_subscription, run_import,
        run_subscribe_import, write_imported_nodes_full_config,
    };
    use crate::config::build_default_config;
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sing-box-tui-{label}-{nonce}"))
    }

    #[test]
    fn raw_subscription_output_cannot_alias_the_active_config() {
        let dir = temp_dir("subscribe-raw-active-alias");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");

        let error = run_subscribe_import(SubscribeImportOptions {
            subscription_url: "this URL must never be parsed".to_string(),
            output: Some(config_path.clone()),
            config_path: config_path.clone(),
            subscription_output: Some(config_path),
            replace_nodes: true,
            include_geosite_rules: false,
            include_tun_mode: false,
            provider_name: None,
            existing_provider_name: None,
        })
        .expect_err("raw subscription output must be rejected before fetching");

        assert!(
            format!("{error:#}").contains("must not alias"),
            "unexpected error: {error:#}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn clash_source_and_output_alias_fails_before_source_read() {
        let dir = temp_dir("clash-source-output-alias");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.json");
        let canary = dir.join("source.yaml");
        let original = b"not valid Clash YAML and must remain unchanged\n";
        fs::write(&canary, original).expect("write source canary");

        let error = run_import(
            &canary,
            Some(&canary),
            true,
            &config_path,
            true,
            false,
            false,
        )
        .expect_err("source/output alias must fail during preflight");

        assert!(
            format!("{error:#}").contains("must not alias"),
            "unexpected error: {error:#}"
        );
        assert_eq!(fs::read(&canary).expect("read canary"), original);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn stdout_only_subscription_import_does_not_resolve_node_quality_path() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("subscribe-stdout-lazy-quality");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("dangling-config.json");
        symlink(dir.join("missing-target.json"), &config_path)
            .expect("create dangling config symlink");

        let error = run_subscribe_import(SubscribeImportOptions {
            subscription_url: "this URL must never reach the network".to_string(),
            output: None,
            config_path,
            subscription_output: None,
            replace_nodes: true,
            include_geosite_rules: false,
            include_tun_mode: false,
            provider_name: None,
            existing_provider_name: None,
        })
        .expect_err("invalid URL must fail without resolving the config-bound database");

        assert!(
            format!("{error:#}").contains("invalid subscription URL"),
            "unexpected error: {error:#}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn distinct_import_output_does_not_open_or_mutate_the_quality_database() {
        let dir = temp_dir("import-output-only-quality");
        fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("active.json");
        let output_path = dir.join("preview.json");
        let database_path = dir.join("active.json.sing-box-tui.sqlite3");
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&build_default_config(vec![json!({
                "type": "trojan", "tag": "node-a", "server": "old.example",
                "server_port": 443, "password": "old-secret"
            })]))
            .expect("serialize active config"),
        )
        .expect("write active config");

        write_imported_nodes_full_config(
            &config_path,
            &output_path,
            vec![json!({
                "type": "trojan", "tag": "node-a", "server": "preview.example",
                "server_port": 443, "password": "preview-secret"
            })],
            true,
            false,
            false,
        )
        .expect("write preview config");

        assert!(output_path.exists());
        assert!(
            !database_path.exists(),
            "a distinct preview output must not initialize node-quality storage"
        );
        assert!(
            fs::read_to_string(&config_path)
                .expect("read active config")
                .contains("old.example")
        );
        let _ = fs::remove_dir_all(dir);
    }

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
